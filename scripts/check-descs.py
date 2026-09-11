import json

with open(r'D:\GitHub\nd-organizer\manifest.json') as f:
    data = json.load(f)

for key in data:
    if isinstance(data[key], dict) and 'properties' in data[key]:
        props = data[key]['properties']
        for k, v in props.items():
            desc = v.get('description', '')
            title = v.get('title', k)
            # Check for vague descriptions
            vague = ['optional', 'required', 'enable', 'disable', 'set', 'configure']
            is_vague = any(w in desc.lower() for w in vague) and len(desc) < 80
            if is_vague:
                print(f"VAGUE: {k} ({title})")
                print(f"  -> {desc}")
                print()
