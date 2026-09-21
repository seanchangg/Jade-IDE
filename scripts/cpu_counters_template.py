"""Rewrite a GUI-saved CPU Counters template for headless use: time
sampling, the trigger event kept but unused, and optionally a new event
list cloned from the first saved event.

    python3 scripts/cpu_counters_template.py in.tracetemplate out.tracetemplate [EVENT,EVENT,...]

Event mnemonics come from Apple's catalog (`/usr/share/kpep`, or
`kpep_db_create`). At most eight programmable events fit; the fixed cycle
and instruction counters are separate."""
import plistlib, json, base64, sys
src, dst = sys.argv[1], sys.argv[2]
events = sys.argv[3].split(',') if len(sys.argv) > 3 else None
p = plistlib.load(open(src, 'rb')); objs = p['$objects']
idx = next(i for i, o in enumerate(objs) if isinstance(o, bytes) and o[:1] == b'{' and b'allEventsAndFormulas' in o)
cfg = json.loads(objs[idx])
cfg['sampleByTime'] = True
cfg.setdefault('pmiEventAliasOrMnemonic', 'ARM_BR_MIS_PRED')
if events:
    proto = plistlib.loads(base64.b64decode(cfg['allEventsAndFormulas'][0]))
    po = proto['$objects']
    mn = next(i for i, o in enumerate(po) if isinstance(o, str) and o.isupper() and '_' in o)
    ds = next(i for i, o in enumerate(po) if isinstance(o, str) and i != mn and o not in ('$null',) and not o.startswith('_') and len(o) > 3)
    print("prototype keys:", [o for o in po if isinstance(o, str)][:12])
    blobs = []
    for e in events:
        b = plistlib.loads(base64.b64decode(cfg['allEventsAndFormulas'][0]))
        b['$objects'][mn] = e; b['$objects'][ds] = e
        blobs.append(base64.b64encode(plistlib.dumps(b, fmt=plistlib.FMT_BINARY)).decode())
    cfg['allEventsAndFormulas'] = blobs
objs[idx] = json.dumps(cfg).encode()
plistlib.dump(p, open(dst, 'wb'), fmt=plistlib.FMT_BINARY)
print("wrote", dst, "sampleByTime", cfg['sampleByTime'], "events", len(cfg['allEventsAndFormulas']))
