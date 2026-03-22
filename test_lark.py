#!/usr/bin/env python3
"""
Lark WebSocket 调试脚本

使用方法:
1. source .venv/bin/activate
2. python3 test_lark.py
3. 在 Lark 群中 @机器人发送消息，查看输出
"""

import json
import os
import sys
import threading
import time

import yaml
from lark_oapi.core.enum import LogLevel
from lark_oapi.event.dispatcher_handler import EventDispatcherHandlerBuilder
from lark_oapi.ws.client import Client as WSClient


class LarkDebugger:
    def __init__(self, app_id: str, app_secret: str):
        self.app_id = app_id
        self.app_secret = app_secret
        self.bot_id = None
        self.running = True
        
    def get_bot_info(self):
        """通过 HTTP API 获取 bot 信息"""
        import aiohttp
        import asyncio
        
        async def fetch():
            async with aiohttp.ClientSession() as session:
                # 获取 token
                async with session.post(
                    "https://open.feishu.cn/open-apis/auth/v3/app_access_token/internal",
                    json={"app_id": self.app_id, "app_secret": self.app_secret}
                ) as resp:
                    data = await resp.json()
                    token = data["app_access_token"]
                
                # 获取 bot 信息
                async with session.get(
                    "https://open.feishu.cn/open-apis/bot/v3/info",
                    headers={"Authorization": f"Bearer {token}"}
                ) as resp:
                    data = await resp.json()
                    return data["bot"]
        
        return asyncio.run(fetch())
    
    def analyze_message(self, event: dict) -> None:
        """分析消息内容"""
        event_data = event.get("event", {})
        message = event_data.get("message", {})
        
        chat_type = message.get("chat_type", "unknown")
        
        try:
            content = json.loads(message.get("content", "{}"))
            text = content.get("text", "")
        except:
            text = message.get("content", "")
        
        print(f"\n{'='*60}")
        print(f"📨 收到消息")
        print(f"{'='*60}")
        print(f"聊天类型: {chat_type}")
        print(f"消息内容: {repr(text)}")
        
        # 检查 mention
        if self.bot_id:
            expected = f'<at user_id="{self.bot_id}">'
            is_mentioned = expected in text
            
            print(f"\n🔍 mention 检测:")
            print(f"   Bot Open ID: {self.bot_id}")
            print(f"   代码检测格式: {repr(expected)}")
            print(f"   是否匹配: {'✅ 是' if is_mentioned else '❌ 否'}")
            
            if chat_type == "group":
                print(f"\n📋 mention_only 逻辑分析:")
                print(f"   mention_only=true 且 is_group=true 且 is_mentioned={is_mentioned}")
                if is_mentioned:
                    print("   ✅ 结果: 消息会被处理")
                else:
                    print("   ❌ 结果: 消息会被忽略")
    
    def handle_message_event(self, event):
        """处理消息事件"""
        try:
            print(f"\n{'='*60}")
            print("📦 收到事件")
            print(f"{'='*60}")
            
            # 打印 event 对象的所有属性
            print(f"event 类型: {type(event)}")
            print(f"event 属性: {[a for a in dir(event) if not a.startswith('_')]}")
            
            # 获取 event 数据
            if hasattr(event, 'event'):
                raw_event = event.event
                print(f"\nevent.event 类型: {type(raw_event)}")
                print(f"event.event 内容: {raw_event}")
                
                if isinstance(raw_event, str):
                    event_dict = json.loads(raw_event)
                else:
                    event_dict = raw_event
            else:
                event_dict = event
            
            print(f"\n解析后的事件: {json.dumps(event_dict, indent=2, ensure_ascii=False)}")
            self.analyze_message(event_dict)
        except Exception as e:
            print(f"处理事件出错: {e}")
            import traceback
            traceback.print_exc()
    
    def run(self):
        """运行调试"""
        print("="*60)
        print("Lark WebSocket 调试工具")
        print("="*60)
        print(f"App ID: {self.app_id[:15]}...")
        
        # 获取 bot 信息
        print("\n获取 Bot 信息...")
        try:
            bot_info = self.get_bot_info()
            self.bot_id = bot_info.get("open_id")
            print(f"✅ Bot: {bot_info.get('app_name', 'N/A')}")
            print(f"✅ Bot Open ID: {self.bot_id}")
        except Exception as e:
            print(f"⚠️ 获取 Bot 信息失败: {e}")
            self.bot_id = input("请手动输入 Bot Open ID: ").strip()
        
        # 创建事件处理器
        print("\n启动 WebSocket 连接...")
        
        # 使用 Builder 创建 Handler，并注册事件处理器
        handler_builder = EventDispatcherHandlerBuilder(
            encrypt_key="",
            verification_token=""
        )
        
        # 注册 im.message.receive_v1 事件处理器
        handler_builder.register_p2_customized_event(
            "im.message.receive_v1",
            self.handle_message_event
        )
        
        handler = handler_builder.build()
        
        # 创建 WebSocket 客户端
        ws_client = WSClient(
            app_id=self.app_id,
            app_secret=self.app_secret,
            log_level=LogLevel.INFO,
            event_handler=handler
        )
        
        print("\n✅ WebSocket 连接成功")
        print("请在 Lark 群中 @机器人发送消息")
        print("按 Ctrl+C 退出\n")
        
        # 在新线程中运行 WebSocket
        def run_ws():
            try:
                ws_client.start()
            except Exception as e:
                if self.running:
                    print(f"\n❌ WebSocket 错误: {e}")
        
        thread = threading.Thread(target=run_ws, daemon=True)
        thread.start()
        
        # 主线程保持运行
        try:
            while self.running:
                time.sleep(1)
        except KeyboardInterrupt:
            print("\n\n👋 正在退出...")
            self.running = False


def main():
    # 读取配置
    app_id = os.environ.get("LARK_APP_ID")
    app_secret = os.environ.get("LARK_APP_SECRET")
    
    if not app_id or not app_secret:
        try:
            with open("config.yaml") as f:
                config = yaml.safe_load(f)
                for ch in config.get("channels", []):
                    if "lark" in ch:
                        app_id = ch["lark"]["app_id"]
                        app_secret = ch["lark"]["app_secret"]
                        print("✅ 从 config.yaml 读取配置")
                        break
        except Exception as e:
            print(f"读取配置失败: {e}")
    
    if not app_id or not app_secret:
        print("❌ 错误: 需要提供 LARK_APP_ID 和 LARK_APP_SECRET")
        print("\n方法1: 设置环境变量")
        print("  export LARK_APP_ID=xxx")
        print("  export LARK_APP_SECRET=xxx")
        print("\n方法2: 确保 config.yaml 中有 Lark 配置")
        sys.exit(1)
    
    debugger = LarkDebugger(app_id, app_secret)
    debugger.run()


if __name__ == "__main__":
    main()
