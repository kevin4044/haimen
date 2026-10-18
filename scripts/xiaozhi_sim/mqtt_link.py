#!/usr/bin/env python3
"""MQTT 链路封装（模拟器用）

把 paho-mqtt 的异步回调模型包装成「同步可读的消息队列」，便于在主循环里使用。

设计取舍：
- 用 paho 的 loop_start() 后台线程，主线程只从 queue 取消息
- 回调里只做入队，不做业务逻辑（避免阻塞 paho 的 IO 线程）
- 提供 context manager，保证退出时正确断开
"""

from __future__ import annotations

import queue
import ssl
import threading
import time
from dataclasses import dataclass, field
from typing import Optional

try:
    import paho.mqtt.client as mqtt
except ImportError as exc:  # pragma: no cover
    raise SystemExit(
        "缺少 paho-mqtt。请安装：\n"
        "  <python> -m pip install paho-mqtt"
    ) from exc


@dataclass
class MqttMessage:
    """一条收到的 MQTT 消息"""

    topic: str
    payload: bytes

    def text(self) -> str:
        return self.payload.decode("utf-8", errors="replace")


@dataclass
class MqttLink:
    """同步风格的 MQTT 连接

    用法：
        with MqttLink("192.168.110.34", 1883, "vdr_runtime", "vdr_runtime",
                      client_id="sim-84c7bb599364") as link:
            link.subscribe("haimen/xiaozhi/#")
            link.publish("haimen/xiaozhi/xxx/say", {"text": "你好"})
            msg = link.get(timeout=5)
    """

    host: str
    port: int = 1883
    username: Optional[str] = None
    password: Optional[str] = None
    client_id: str = "haimen-sim"
    keepalive: int = 60
    use_tls: bool = False

    _client: Optional[mqtt.Client] = field(default=None, init=False, repr=False)
    _inbox: "queue.Queue[MqttMessage]" = field(
        default_factory=queue.Queue, init=False, repr=False
    )
    _connected: threading.Event = field(
        default_factory=threading.Event, init=False, repr=False
    )
    _subscribed: dict[str, int] = field(default_factory=dict, init=False, repr=False)
    _lock: threading.Lock = field(default_factory=threading.Lock, init=False, repr=False)

    # ── 生命周期 ──────────────────────────────────────────────

    def connect(self, timeout: float = 10.0) -> None:
        c = mqtt.Client(
            callback_api_version=mqtt.CallbackAPIVersion.VERSION2,
            client_id=self.client_id,
            protocol=mqtt.MQTTv311,
        )
        if self.username is not None:
            c.username_pw_set(self.username, self.password)
        if self.use_tls:
            c.tls_set(cert_reqs=ssl.CERT_NONE)
            c.tls_insecure_set(True)

        c.on_connect = self._on_connect
        c.on_message = self._on_message
        c.on_disconnect = self._on_disconnect

        self._client = c
        c.connect_async(self.host, self.port, keepalive=self.keepalive)
        c.loop_start()

        if not self._connected.wait(timeout):
            c.loop_stop()
            raise TimeoutError(
                f"{timeout}s 内没连上 MQTT {self.host}:{self.port}"
            )

        # 重放连接前登记过的订阅
        with self._lock:
            topics = list(self._subscribed.items())
        for topic, qos in topics:
            c.subscribe(topic, qos=qos)

    def close(self) -> None:
        if self._client is None:
            return
        try:
            self._client.disconnect()
            self._client.loop_stop()
        finally:
            self._client = None
            self._connected.clear()

    def __enter__(self) -> "MqttLink":
        self.connect()
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # ── 收发 ──────────────────────────────────────────────────

    def subscribe(self, topic: str, qos: int = 1) -> None:
        with self._lock:
            self._subscribed[topic] = qos
        if self._client is not None:
            self._client.subscribe(topic, qos=qos)

    def publish(self, topic: str, payload, qos: int = 1) -> None:
        """payload 可以是 bytes / str / dict（dict 会按 JSON 序列化）"""
        import json
        if isinstance(payload, (dict, list)):
            data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        elif isinstance(payload, str):
            data = payload.encode("utf-8")
        else:
            data = payload
        if self._client is None:
            raise RuntimeError("MQTT 未连接，先调用 connect()")
        info = self._client.publish(topic, data, qos=qos)
        if info.rc != mqtt.MQTT_ERR_SUCCESS:
            raise RuntimeError(f"publish 失败 rc={info.rc}")

    def get(self, timeout: Optional[float] = 5.0) -> Optional[MqttMessage]:
        """取一条消息，超时返回 None"""
        try:
            if timeout is None:
                return self._inbox.get()
            return self._inbox.get(timeout=timeout)
        except queue.Empty:
            return None

    def drain(self) -> list[MqttMessage]:
        out = []
        while True:
            try:
                out.append(self._inbox.get_nowait())
            except queue.Empty:
                return out

    def wait_for(self, predicate, timeout: float = 10.0):
        """等待第一条满足 predicate(msg) 的消息"""
        deadline = time.monotonic() + timeout
        while True:
            remain = deadline - time.monotonic()
            if remain <= 0:
                return None
            msg = self.get(timeout=remain)
            if msg is None:
                return None
            if predicate(msg):
                return msg

    # ── paho 回调（只在 IO 线程里入队，不做业务）────────────────

    def _on_connect(self, client, userdata, flags, reason_code, properties=None):
        rc = getattr(reason_code, "value", reason_code)
        if rc == 0:
            with self._lock:
                topics = list(self._subscribed.items())
            for topic, qos in topics:
                client.subscribe(topic, qos=qos)
            self._connected.set()
        else:
            # 连接被拒（鉴权失败等）也要让 connect() 解除阻塞
            self._connected.set()

    def _on_message(self, client, userdata, msg):
        self._inbox.put(MqttMessage(topic=msg.topic, payload=msg.payload))

    def _on_disconnect(self, client, userdata, flags, reason_code, properties=None):
        self._connected.clear()


if __name__ == "__main__":
    # 冒烟测试：连上 broker，订阅，然后自己发一条自己收
    import sys

    host = sys.argv[1] if len(sys.argv) > 1 else "192.168.110.34"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 1883
    user = sys.argv[3] if len(sys.argv) > 3 else "vdr_runtime"
    pwd = sys.argv[4] if len(sys.argv) > 4 else "vdr_runtime"

    with MqttLink(host, port, user, pwd, client_id="haimen-sim-smoke") as link:
        print(f"已连接 {host}:{port}")
        link.subscribe("haimen/test/#")
        time.sleep(0.5)
        link.publish("haimen/test/ping", {"ok": True})
        msg = link.wait_for(lambda m: m.topic == "haimen/test/ping", timeout=5)
        print(f"收到: {msg.topic} {msg.text()}" if msg else "❌ 没收到自己发的消息")
