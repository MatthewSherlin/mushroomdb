# benchmarks/agent-tasks/test_agent_harness.py
"""Harness v2 unit tests. Run: bindings/python/.venv/bin/python -m pytest benchmarks/agent-tasks/test_agent_harness.py -v"""
from __future__ import annotations
import json, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))


def test_usage_of_reports_cache_hit_ratio():
    from run import usage_of
    u = usage_of({"usage": {"input_tokens": 100, "output_tokens": 50,
                            "cache_read_input_tokens": 700, "cache_creation_input_tokens": 200}})
    assert u["total_tokens"] == 1050
    assert abs(u["cache_hit_ratio"] - 700 / 1000) < 1e-9


def test_usage_of_zero_context_has_zero_ratio():
    from run import usage_of
    assert usage_of({"usage": {}})["cache_hit_ratio"] == 0.0


def test_parse_stream_counts_tool_search_and_graph_calls(tmp_path):
    from run import parse_stream
    events = [
        {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "ToolSearch", "input": {"query": "select:mcp__mushroomdb__explore"}},
            {"type": "tool_use", "name": "mcp__mushroomdb__explore", "input": {"target": "x"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "mushroomdb explore render_map"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "/opt/bin/mushroomdb query 'MATCH (n) RETURN n'"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "grep -rn mushroomdb ."}},
            {"type": "tool_use", "name": "Grep", "input": {"pattern": "foo"}},
        ]}},
        {"type": "result", "result": "done", "usage": {}},
    ]
    p = tmp_path / "s.jsonl"
    p.write_text("\n".join(json.dumps(e) for e in events))
    parsed = parse_stream(p)
    assert parsed["tool_search_calls"] == 1
    assert parsed["graph_calls"] == 3
    assert parsed["tool_calls"]["Grep"] == 1


def test_cell_command_arm_d_has_no_mcp_and_no_prefix():
    from run import cell_command, MCP_TOOL, SUBJECT_D
    cmd, cwd = cell_command("D", "find the retry policy", 30, None)
    assert cwd == SUBJECT_D
    assert "--strict-mcp-config" in cmd
    tools = cmd[cmd.index("--allowedTools") + 1]
    assert MCP_TOOL not in tools
    assert "Edit" in tools and "Write" in tools
    assert cmd[cmd.index("--max-turns") + 1] == "30"
    assert cmd[2] == "find the retry policy"


def test_cell_command_arm_c_prefixes_and_allows_mcp():
    from run import cell_command, MCP_TOOL
    cmd, _ = cell_command("C", "q", 12, None)
    assert cmd[2] == "/mushroom q"
    assert MCP_TOOL in cmd[cmd.index("--allowedTools") + 1]


def test_cell_command_cwd_override_wins(tmp_path):
    from run import cell_command
    _, cwd = cell_command("A", "q", 30, tmp_path)
    assert cwd == tmp_path
