import json
from pathlib import Path


arguments = json.loads(Path("args.json").read_text())
citation = arguments.get("citation")
print(json.dumps({"citation": citation, "found": bool(citation)}))
