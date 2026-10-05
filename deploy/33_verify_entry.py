#!/usr/bin/env python3
"""ywl/zzp: python3 ~/codex-proxy-rs/deploy/33_verify_entry.py
Existing isolated candidate only. Proves a denied route creates an actionable
ops_event, not a model execution. Does not modify any key/group/account policy.
Result: .runtime/30_upstream_acceptance/full-repair.json. Python3 stdlib.
"""
import json,os,subprocess,urllib.request,urllib.error
from pathlib import Path
os.umask(0o077)
rt=Path('/home/zzp/codex-proxy-rs/.runtime/30_upstream_acceptance');out=rt/'full-repair.json'
record=json.loads(out.read_text()) if out.exists() else {}
source=(rt/'binary-source.txt').read_text().strip()
previous=record.get('entry_diagnostics')
assert not (previous and previous.get('passed') and previous.get('source')==source),'Already accepted source; do not replay'
def sql(statement):
 return subprocess.check_output(['podman','exec','-i','cpr-upstream30-pg','sh','-c','PGPASSWORD="$POSTGRES_PASSWORD" psql -XAt -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB"'],input=statement.encode()).decode().strip()
key_id='key_bf9318e471d05b0fafb21f46b8af0099';key=sql("SELECT key FROM client_api_keys WHERE id='"+key_id+"';")
started=sql('SELECT now();')
request=urllib.request.Request('http://127.0.0.1:18140/v1/responses',data=json.dumps({'model':'gpt-6-astra','input':'Reply ENTRY_DIAGNOSTIC_SHOULD_NOT_RUN','stream':False}).encode(),headers={'Authorization':'Bearer '+key,'Content-Type':'application/json'})
try:
 with urllib.request.urlopen(request,timeout=30) as response:status=response.status;body=json.load(response)
except urllib.error.HTTPError as error:status=error.code;body=json.loads(error.read().decode())
receipt={'source':source,'http':status,'client_body':body,'expected_http':503,'passed':False}
if previous:record.setdefault('prior_entry_diagnostics',[]).append(previous)
record['entry_diagnostics']=receipt;out.write_text(json.dumps(record,indent=2)+'\n')
assert status==503, 'Expected the existing model policy to reject this route before execution'
rows=json.loads(sql("SELECT COALESCE(json_agg(message::jsonb),'[]') FROM ops_events WHERE component='request_entry' AND operation='reject' AND created_at>='"+started+"'::timestamptz AND message::jsonb->>'clientKeyId'='"+key_id+"';"))
assert len(rows)==1,'Expected one entry rejection event'
event=rows[0];receipt['event']=event
assert event['requestedModel']=='gpt-6-astra' and event['endpoint']=='/v1/responses'
assert event['routingGroupRefs'] and event['routingGroupNames'] and event['configRevision']>0
assert event['routingError']['code']=='no_capable_provider'
assert event['routingError']['providerExclusions']['openai']=='account_model_policy'
receipt['model_request_rows']=int(sql("SELECT count(*) FROM model_requests WHERE id='"+event['requestId']+"';"));assert receipt['model_request_rows']==0
receipt['passed']=True;out.write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(receipt))
