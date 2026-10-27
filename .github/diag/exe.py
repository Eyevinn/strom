# TEMPORARY: issue #1136 diag. Never merge. Prints the latency test executable.
import json, sys
for line in sys.stdin:
    try:
        m = json.loads(line)
    except Exception:
        continue
    exe = m.get("executable") if m.get("reason") == "compiler-artifact" else None
    if exe and "latency_diagnosis_test" in exe:
        print(exe.replace("\\", "/"))
