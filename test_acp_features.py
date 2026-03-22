import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Dict, List, Any, Optional

class AcpTester:
    def __init__(self, command: List[str]):
        self.command = command
        self.process = None
        self.responses: Dict[int, Any] = {}
        self.updates: List[Any] = []
        self.done_event = threading.Event()
        self.error: Optional[str] = None
        self.next_id = 1
        self.info = {
            "agent_info": {},
            "capabilities": {},
            "auth_methods": [],
            "models": [],
            "modes": [],
            "config_options": [],
            "commands": []
        }

    def log(self, msg: str):
        print(f"[*] {msg}")

    def read_stdout(self):
        if not self.process or not self.process.stdout:
            return
            
        for line in self.process.stdout:
            line = line.rstrip("\n")
            if not line:
                continue
            try:
                # Robustly find JSON in the line or skip non-JSON prefix/suffix
                if not line.strip().startswith("{"):
                    # self.log(f"[RAW LOG] {line}")
                    continue
                
                data = json.loads(line)
                method = data.get("method")
                req_id = data.get("id")

                if isinstance(req_id, int):
                    self.responses[req_id] = data
                
                if method == "session/update":
                    update = data.get("params", {}).get("update", {})
                    self.updates.append(update)
                    self._parse_update(update)
            except Exception as e:
                # self.log(f"[PARSE ERROR] {e} for line: {line}")
                pass

    def _parse_update(self, update: Any):
        u_type = update.get("sessionUpdate")
        if u_type == "available_commands_update":
            self.info["commands"] = update.get("availableCommands", [])
        elif u_type == "config_option_update":
            self.info["config_options"] = update.get("configOptions", [])
        elif u_type == "available_models_update": # Some impls might use this
             self.info["models"] = update.get("availableModels", [])

    def send(self, method: str, params: Any = None) -> int:
        req_id = self.next_id
        self.next_id += 1
        msg = {
            "jsonrpc": "2.0",
            "method": method,
            "id": req_id
        }
        if params is not None:
            msg["params"] = params
            
        payload = json.dumps(msg)
        if self.process and self.process.stdin:
            self.process.stdin.write(payload + "\n")
            self.process.stdin.flush()
        return req_id

    def wait_for_response(self, req_id: int, timeout: float = 10.0) -> Optional[Any]:
        start = time.time()
        while time.time() - start < timeout:
            if req_id in self.responses:
                return self.responses[req_id]
            time.sleep(0.1)
        return None

    def run_test(self):
        self.log(f"Starting process: {' '.join(self.command)}")
        try:
            self.process = subprocess.Popen(
                self.command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                bufsize=1
            )
        except Exception as e:
            print(f"[!] Failed to start process: {e}")
            return

        threading.Thread(target=self.read_stdout, daemon=True).start()

        # 1. Initialize
        self.log("Sending 'initialize'...")
        rid = self.send("initialize", {
            "clientInfo": {"name": "acp-capability-tester", "version": "1.0"},
            "protocolVersion": 1,
            "capabilities": {}
        })
        resp = self.wait_for_response(rid, timeout=30.0)
        if not resp or "result" not in resp:
            print(f"[!] Initialize failed or timed out: {resp}")
            self.process.terminate()
            return

        result = resp["result"]
        self.info["agent_info"] = result.get("agentInfo", {})
        self.info["capabilities"] = result.get("agentCapabilities", {})
        self.info["auth_methods"] = result.get("authMethods", [])

        # 2. Session New
        self.log("Sending 'session/new'...")
        rid = self.send("session/new", {
            "cwd": str(Path(os.getcwd()).resolve()),
            "mcpServers": []
        })
        resp = self.wait_for_response(rid, timeout=15.0)
        if not resp or "result" not in resp:
            print(f"[!] session/new failed or timed out: {resp}")
            self.process.terminate()
            return

        res = resp["result"]
        session_id = res.get("sessionId")
        self.info["models"] = res.get("models", {}).get("availableModels", [])
        self.info["modes"] = res.get("modes", {}).get("availableModes", [])
        self.info["config_options"] = res.get("configOptions", [])

        self.log(f"Session established: {session_id}")
        
        # 3. Wait for async updates (like commands)
        self.log("Waiting 5s for asynchronous updates (commands, etc.)...")
        time.sleep(5)

        # 4. Try to trigger help if no commands found
        if not self.info["commands"]:
            self.log("No commands advertised yet. Sending '/help' to trigger updates...")
            self.send("session/prompt", {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "/help"}]
            })
            time.sleep(5)

        self.process.terminate()
        self.report()

    def report(self):
        print("\n" + "="*60)
        print(" ACP CAPABILITY REPORT ")
        print("="*60)
        
        ai = self.info["agent_info"]
        print(f"\n[Agent Info]")
        print(f"  Name:    {ai.get('name', 'N/A')}")
        print(f"  Title:   {ai.get('title', 'N/A')}")
        print(f"  Version: {ai.get('version', 'N/A')}")

        cap = self.info["capabilities"]
        print(f"\n[Capabilities]")
        print(f"  Load Session: {cap.get('loadSession', False)}")
        print(f"  Prompt:       {json.dumps(cap.get('promptCapabilities', {}))}")
        
        print(f"\n[Auth Methods]")
        for am in self.info["auth_methods"]:
            print(f"  - {am.get('name')} ({am.get('id')}): {am.get('description')}")

        print(f"\n[Available Models] ({len(self.info['models'])})")
        for m in self.info["models"][:10]: # Limit display
            print(f"  - {m.get('modelId')}: {m.get('name')}")
        if len(self.info["models"]) > 10:
            print(f"    ... and {len(self.info['models']) - 10} more")

        print(f"\n[Available Modes]")
        for m in self.info["modes"]:
            print(f"  - {m.get('name')} ({m.get('id')})")

        print(f"\n[Config Options]")
        for opt in self.info["config_options"]:
            print(f"  - {opt.get('name')} ({opt.get('id')}): current={opt.get('currentValue')}")

        print(f"\n[Slash Commands] ({len(self.info['commands'])})")
        if not self.info["commands"]:
            print("  (None advertised via available_commands_update)")
        else:
            for cmd in self.info["commands"]:
                print(f"  - {cmd.get('name')}: {cmd.get('description')}")
        
        print("\n" + "="*60 + "\n")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python test_acp_features.py <command> [args...]")
        print("Example: python test_acp_features.py copilot --acp")
        sys.exit(1)
        
    tester = AcpTester(sys.argv[1:])
    tester.run_test()
