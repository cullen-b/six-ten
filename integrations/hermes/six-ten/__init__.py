"""six-ten: coordinates file edits between coding agents sharing one git checkout."""

import json
import os
import subprocess

# Notes for the model, keyed by tool_call_id, appended to that call's result.
_pending = {}


def _cwd():
    terminal = os.environ.get("TERMINAL_CWD", "")
    return terminal if os.path.isabs(terminal) else os.getcwd()


def _call(payload):
    """Runs `six-ten hook hermes-plugin`; returns (exit code, note, stderr)."""
    payload["cwd"] = _cwd()
    try:
        proc = subprocess.run(
            ["six-ten", "hook", "hermes-plugin"],
            input=json.dumps(payload, default=str),
            capture_output=True,
            text=True,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired):
        # six-ten missing or stuck: never block the agent.
        return 0, None, ""
    note = None
    if proc.stdout.strip():
        try:
            note = json.loads(proc.stdout).get("note")
        except ValueError:
            pass
    return proc.returncode, note, proc.stderr.strip()


def _stash(tool_call_id, note):
    if note:
        _pending[tool_call_id] = "\n\n".join(n for n in (_pending.get(tool_call_id), note) if n)


def _pre_tool_call(tool_name, args=None, session_id="", tool_call_id="", **_):
    code, note, err = _call({"hook_event_name": "pre_tool_call", "tool_name": tool_name,
                             "tool_input": args or {}, "session_id": session_id})
    if code == 2 and err:
        return {"action": "block", "message": err}
    _stash(tool_call_id, note)
    return None


def _post_tool_call(tool_name, args=None, session_id="", tool_call_id="", **_):
    _, note, _ = _call({"hook_event_name": "post_tool_call", "tool_name": tool_name,
                        "tool_input": args or {}, "session_id": session_id})
    _stash(tool_call_id, note)


def _transform_tool_result(tool_name, args=None, result=None, tool_call_id="", **_):
    note = _pending.pop(tool_call_id, None)
    if not note:
        return None
    text = result if isinstance(result, str) else json.dumps(result, default=str)
    return f"{text}\n\n{note}"


def _pre_llm_call(session_id="", **_):
    _, note, _ = _call({"hook_event_name": "pre_llm_call", "session_id": session_id})
    return {"context": note} if note else None


def _end(session_id="", **_):
    _call({"hook_event_name": "on_session_end", "session_id": session_id})


def register(ctx):
    ctx.register_hook("pre_tool_call", _pre_tool_call)
    ctx.register_hook("post_tool_call", _post_tool_call)
    ctx.register_hook("transform_tool_result", _transform_tool_result)
    ctx.register_hook("pre_llm_call", _pre_llm_call)
    ctx.register_hook("on_session_end", _end)
    ctx.register_hook("on_session_finalize", _end)
