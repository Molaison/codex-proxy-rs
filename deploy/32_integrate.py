#!/usr/bin/env python3
"""ywl/zzp: python3 ~/codex-proxy-rs/.runtime/30_upstream_acceptance/source/deploy/32_integrate.py catalog_iso_prodata
Only the user-designated isolated18382 instance is permitted. Python3 + PyYAML.
Refresh three Web catalogs and verify both existing Full keys and metadata.
Historical production/18140 stages are frozen and rejected before any access.
Receipt: .runtime/30_upstream_acceptance/integration.json; no credentials emitted.
"""
import json,os,subprocess,sys,urllib.request,urllib.error,http.cookiejar
from pathlib import Path
import yaml
os.umask(0o077)
stage=sys.argv[1];assert stage in ('config','bridge','catalog','catalog_production','catalog_iso_prodata','live')
if stage!='catalog_iso_prodata':raise SystemExit('PRODUCTION_FROZEN: use catalog_iso_prodata on18382; previous production/18140 stages are disabled')
root=Path('/home/zzp/codex-proxy-rs');rt=root/'.runtime/30_upstream_acceptance';out=rt/'integration.json'
record=json.loads(out.read_text()) if out.exists() else {'base_source':'a72a7f5c','production_changed':False}
prod='codex-proxy-rs_postgres_1';candidate='cpr-iso-prodata_postgres_1'
catalog_key='prodata_tool_catalog'
qa_group='grp_01a0e90b82a37015b19bafcc58baa621';qa_account='acct_01a0e90b82c073d1ba137ae2edc984d1'
units=[json.loads(Path('/home/zzp/.local/share/codex-chatgpt-web/full-'+str(i)+'/public-test.json').read_text()) for i in (1,2)]
def sql(db,statement):
 return subprocess.check_output(['podman','exec','-i',db,'sh','-c','PGPASSWORD="$POSTGRES_PASSWORD" psql -XAt -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB"'],input=statement.encode()).decode().strip()
if stage=='config':
 before=[];changes=['BEGIN;'];changed=False
 for u in units:
  account=u['account'];key_id=u['key_id']
  current=json.loads(sql(candidate,"SELECT model_access_json FROM provider_accounts WHERE id='"+account+"';"))
  desired=json.loads(sql(prod,"SELECT model_access_json FROM provider_accounts WHERE id='"+account+"';"))
  assert desired['mode']=='allowlist' and all(m.endswith('-tools') for m in desired['models'])
  before.append({'account':account,'model_access':current})
  if current!=desired:
   changes.append("UPDATE provider_accounts SET model_access_json='"+json.dumps(desired).replace("'","''")+"'::jsonb,credential_revision=credential_revision+1,updated_at=now() WHERE id='"+account+"';");changed=True
  assert sql(prod,"SELECT count(*) FROM client_api_key_groups WHERE client_api_key_id='"+key_id+"' AND account_group_id='"+qa_group+"';")=='1'
  if sql(candidate,"SELECT count(*) FROM client_api_key_groups WHERE client_api_key_id='"+key_id+"' AND account_group_id='"+qa_group+"';")=='0':
   changes.append("INSERT INTO client_api_key_groups(client_api_key_id,account_group_id,created_at) VALUES('"+key_id+"','"+qa_group+"',now());");changed=True
 if changed:changes.append('UPDATE runtime_settings SET config_revision=config_revision+1,updated_at=now();')
 changes.append('COMMIT;');sql(candidate,'\n'.join(changes))
 record['tool_config']={'applied':True,'changed':changed,'accounts':[u['account'] for u in units],'before':record.get('tool_config',{}).get('before',before),'production_source':'Web PR5 f79e5fc','preserved_candidate_extra_grants':True}
 out.write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record['tool_config']));sys.exit(0)
if stage=='bridge':
 bridges=[]
 for i in (1,2):
  name='cpr-upstream30-full-bridge-'+str(i)
  found=subprocess.call(['podman','container','exists',name])==0
  if not found:
   subprocess.check_call(['podman','run','-d','--name',name,'--network','host','--security-opt','label=disable','--security-opt','no-new-privileges:true','--cap-drop','ALL','--read-only','-v','/home/zzp/.local/share/codex-chatgpt-web/full-'+str(i)+'/cpr:/run/provider:ro','docker.io/alpine/socat@sha256:24220ef2c80a2a421ea08e4624488e985330c421b6aa3329bae14b0933a1d403','TCP-LISTEN:'+str(17970+i)+',fork,reuseaddr,bind=127.0.0.1','UNIX-CONNECT:/run/provider/provider.sock'])
  bridges.append({'name':name,'port':17970+i,'network':'host','existing':found})
 record['candidate_bridges']=bridges
 record['catalog_refresh_failure_cause']='Full ports 17971/17972 exist only in production netns; host-network candidate connection refused. Added candidate-only loopback bridges to existing authenticated Unix sockets.'
 out.write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(bridges));sys.exit(0)
if stage=='live':
 key=sql(candidate,"SELECT key FROM client_api_keys WHERE id='"+units[0]['key_id']+"';")
 rows=[]
 for authenticated,expected in [(False,401),(True,400)]:
  h={'Content-Type':'application/json'}
  if authenticated:h['Authorization']='Bearer '+key
  try:
   with urllib.request.urlopen(urllib.request.Request('http://127.0.0.1:18140/v1/live',data=b'{}',headers=h),timeout=15) as response:status=response.status
  except urllib.error.HTTPError as error:status=error.code
  rows.append({'authenticated':authenticated,'http':status,'expected':expected,'passed':status==expected})
 record['live_boundary']={'passed':all(r['passed'] for r in rows),'rows':rows,'real_voice_request':False}
 out.write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record['live_boundary']));assert record['live_boundary']['passed'];sys.exit(0)
if stage=='catalog_production':
 assert json.loads(subprocess.check_output(['podman','inspect','codex-proxy-rs_codex-proxy-rs_1']).decode())[0]['Config']['Image']==(rt/'image.txt').read_text().strip()
conf=yaml.safe_load(Path('/home/zzp/cpr-iso-prodata-20261005/deploy/config.yaml').read_text());base='http://127.0.0.1:18382'
jar=http.cookiejar.CookieJar();opener=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
body={'mode':'admin','username':conf['admin']['default_username'],'password':conf['admin']['default_password']}
with opener.open(urllib.request.Request(base+'/api/auth/login',data=json.dumps(body).encode(),headers={'Content-Type':'application/json'}),timeout=30) as response:assert json.load(response)['code']==200
headers={'Content-Type':'application/json','Cookie':'; '.join(c.name+'='+c.value for c in jar)}
for account in [qa_account]+[u['account'] for u in units]:
 try:
  with opener.open(urllib.request.Request(base+'/api/admin/accounts/models/refresh',data=json.dumps({'accountId':account}).encode(),headers=headers),timeout=90) as response: assert json.load(response)['code']==200
 except urllib.error.HTTPError as error:
  failure={'stage':'catalog_refresh','account':account,'http':error.code,'body':error.read().decode()}
  record.setdefault('failed_attempts',[]).append(failure);out.write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(failure));raise
catalog_sources={}
for account in [qa_account]+[u['account'] for u in units]:
 c=json.loads(sql(candidate,"SELECT provider_credentials_json FROM provider_accounts WHERE id='"+account+"';"))
 # These loopback sources exist in the isolated gateway netns, not host networking.
 request='url = '+json.dumps(c['base_url'].rstrip('/')+'/models?client_version=0.160.0')+'\nheader = '+json.dumps('Authorization: Bearer '+c['api_key'])+'\nmax-time = 30\n'
 source=json.loads(subprocess.check_output(['podman','exec','-i','cpr-iso-prodata_codex-proxy-rs_1','curl','-fsS','--config','-'],input=request.encode()).decode())
 catalog_sources[account]={m['slug']:m for m in source['models']}
rows=[]
for u in units:
 key=sql(candidate,"SELECT key FROM client_api_keys WHERE id='"+u['key_id']+"';")
 with urllib.request.urlopen(urllib.request.Request(base+'/v1/models',headers={'Authorization':'Bearer '+key}),timeout=30) as response:models=json.load(response)['data']
 ids=sorted(m['id'] for m in models);row={'key_id':u['key_id'],'models':ids,'plain_count':sum(not m.endswith('-tools') for m in ids),'tools_count':sum(m.endswith('-tools') for m in ids)}
 row['passed']=row['plain_count']==9 and row['tools_count']==9 and 'chatgpt-web/gpt-5.6-sol' in ids and 'chatgpt-web/gpt-5.6-sol-tools' in ids
 with urllib.request.urlopen(urllib.request.Request(base+'/v1/models?client_version=0.160.0',headers={'Authorization':'Bearer '+key}),timeout=30) as response:native={m['slug']:m for m in json.load(response)['models']}
 row['metadata_matches_sources']=all(m in native and all(native[m].get(f)==expected[m].get(f) for f in ('base_instructions','model_messages')) for expected in (catalog_sources[qa_account],catalog_sources[u['account']]) for m in expected)
 plain=native.get('chatgpt-web/gpt-5.6-sol',{});full=native.get('chatgpt-web/gpt-5.6-sol-tools',{})
 row['plain_tools_instructions_differ']=bool(plain.get('base_instructions')) and bool(full.get('base_instructions')) and plain.get('base_instructions')!=full.get('base_instructions')
 row['passed']=row['passed'] and row['metadata_matches_sources'] and row['plain_tools_instructions_differ']
 rows.append(row)
previous=record.get(catalog_key)
if previous and not previous.get('passed'):record.setdefault(catalog_key+'_failed_attempts',[]).append(previous)
record[catalog_key]={'passed':all(r['passed'] for r in rows),'source':(rt/'binary-source.txt').read_text().strip(),'rows':rows};out.write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record[catalog_key]));assert record[catalog_key]['passed']
