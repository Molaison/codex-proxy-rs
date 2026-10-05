#!/usr/bin/env python3
"""Production is frozen. This candidate copy only permits an isolated rehearsal:
python3 ~/codex-proxy-rs/.runtime/30_upstream_acceptance/source/deploy/34_integrated_release.py rehearse
Wait for the model-catalog owner to release cpr-upstream30 first.
Recreates only cpr-upstream30 and two named ephemeral dependency containers.
Keeps candidate.yaml, its image, PG18141 and Redis18142 unchanged. No model calls.
Receipt/log: .runtime/30_upstream_acceptance/release-rehearsal.{json,log}.
Legacy production stages below remain disabled pending a new approved window.
"""
import argparse,datetime,fcntl,json,os,shutil,subprocess,time,urllib.request,urllib.error
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('stage',choices=['rehearse','prepare','deploy','resume','rollback']);p.add_argument('--allow-interrupt',action='store_true');args=p.parse_args();os.umask(0o077)
if args.stage!='rehearse':p.error('PRODUCTION_FROZEN: only isolated rehearse is permitted; --allow-interrupt is not a maintenance-window approval')
root=Path('/home/zzp/codex-proxy-rs');rt=root/'.runtime';candidate=rt/'30_upstream_acceptance';statefile=rt/'34_integrated_release.json';beforefile=rt/'34_integrated_release_before.json';dump=rt/'34_cutover_db.dump'
gateway='codex-proxy-rs_codex-proxy-rs_1';bridges=['codex-proxy-rs_cpa-bridge_1','codex-proxy-rs_sub2api-bridge_1','codex-proxy-rs_chatgpt-web-bridge_1'];dbs=['codex-proxy-rs_postgres_1','codex-proxy-rs_redis_1']
units={'chatgpt-web-pool-bridge.service':'chatgpt-web-pool-bridge','chatgpt-web-cpr-bridge-1.service':'chatgpt-web-local-1-bridge','chatgpt-web-cpr-bridge-2.service':'chatgpt-web-local-2-bridge','chatgpt-web-full-bridge-1.service':'chatgpt-web-full-bridge-1','chatgpt-web-full-bridge-2.service':'chatgpt-web-full-bridge-2'}
fallback='localhost/cpr-ywl:3.19.0-acceptance-89619791'
if args.stage=='rehearse':
 gateway='cpr-upstream30';bridges=[];units={};dbs=['cpr-upstream30-pg','cpr-upstream30-redis']
 statefile=candidate/'release-rehearsal.json'
log=(candidate/'release-rehearsal.log').open('a')
lock=(candidate/'release-rehearsal.lock').open('a');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
def inspect(names):return json.loads(subprocess.check_output(['podman','inspect']+names).decode())
def run(cmd,stdout=None):
 code=subprocess.call(cmd,stdout=stdout or log,stderr=log,stdin=subprocess.DEVNULL)
 if code:raise RuntimeError('Native operation failed (%s); inspect private release log'%code)
def save(phase):
 state['phase']=phase;state['updated_at']=datetime.datetime.now(datetime.timezone.utc).isoformat();statefile.write_text(json.dumps(state,indent=2)+'\n');print(json.dumps({'stage':args.stage,'phase':phase}),flush=True)
def db_identity(items):return {x['Name']:[x['Id'],x['State']['StartedAt']] for x in items if x['Name'] in dbs}
def gateway_dependents(gateway_id):
 ids=subprocess.check_output(['podman','ps','-aq']).decode().split()
 found=[]
 for ident in ids:
  result=subprocess.run(['podman','inspect',ident],stdout=subprocess.PIPE,stderr=log)
  if result.returncode:
   # --rm systemd bridges can disappear after ps during their normal owner stop.
   if subprocess.call(['podman','container','exists',ident])==1:continue
   raise RuntimeError('Container inspection failed for existing '+ident)
  x=json.loads(result.stdout.decode())[0]
  if gateway_id in x.get('Dependencies',[]) or x['HostConfig'].get('NetworkMode') in ['container:'+gateway_id,'container:'+gateway]:found.append(x)
 unknown=[x['Name'] for x in found if x['Name'] not in bridges+list(units.values())]
 assert not unknown,'Unknown dependent containers; coordinate owners first: '+repr(unknown)
 return found
if args.stage=='rehearse':
 state=json.loads(statefile.read_text()) if statefile.exists() else {}
 assert state.get('phase')!='passed','This isolated boundary is already accepted; do not replay'
 assert not state,'Existing incomplete rehearsal: recover recorded containers, do not submit again'
 original=inspect([gateway]+dbs);old=original[0]
 assert old['State']['Running'] and old['Config']['Image']==(candidate/'image.txt').read_text().strip()
 config_bytes=(candidate/'candidate.yaml').read_bytes()
 state={'target':'isolated','image':old['Config']['Image'],'gateway_before':old['Id'],'db_before':db_identity(original),'recovery_create_command':old['Config']['CreateCommand']}
 native=gateway+'-release-native';auto=gateway+'-release-auto'
 state['owned_ephemeral_containers']=[native,auto];save('prepared')
 try:
  # Native --requires is real even without a shared network namespace.
  image='docker.io/alpine/socat@sha256:24220ef2c80a2a421ea08e4624488e985330c421b6aa3329bae14b0933a1d403'
  run(['podman','run','-d','--name',native,'--requires',gateway,'--network','none','--entrypoint','/bin/sh',image,'-c','sleep 300'])
  try:gateway_dependents(old['Id'])
  except AssertionError as error:
   assert native in str(error);state['unknown_dependency_rejected_before_stop']=True
  else:raise AssertionError('Unknown native dependent was not rejected')
  assert inspect([gateway])[0]['State']['Running']
  bridges.append(native)
  found=gateway_dependents(old['Id']);assert [x['Name'] for x in found]==[native]
  assert found[0]['HostConfig']['NetworkMode']=='none';state['native_only_dependency_detected']=True
  bridges.append(auto)
  run(['podman','run','-d','--rm','--name',auto,'--requires',gateway,'--network','none','--entrypoint','/bin/sh',image,'-c','sleep 300'])
  save('draining_isolated_gateway');run(['podman','stop','--time','75',gateway])
  # Owner-driven --rm cleanup matches the observed asynchronous removal boundary.
  run(['podman','stop','--ignore','--time','1',auto])
  remaining=gateway_dependents(old['Id']);assert native in [x['Name'] for x in remaining]
  names=[x['Name'] for x in remaining]
  run(['podman','stop','--ignore','--time','1']+names);run(['podman','rm','--ignore']+names)
  assert not gateway_dependents(old['Id']);state['auto_removed_and_native_dependencies_cleared']=True
  save('recreating_isolated_gateway');run(['podman','rm',gateway]);run(old['Config']['CreateCommand'])
  for _ in range(45):
   try:
    with urllib.request.urlopen('http://127.0.0.1:18140/healthz',timeout=3) as response:
     if response.status==204:break
   except (OSError,urllib.error.URLError):pass
   time.sleep(2)
  else:raise RuntimeError('Recreated isolated gateway failed readiness; preserve receipt for recovery')
  now=inspect([gateway]+dbs);state['gateway_after']=now[0]['Id'];state['db_after']=db_identity(now)
  assert state['gateway_after']!=state['gateway_before'] and now[0]['Config']['Image']==state['image']
  assert state['db_before']==state['db_after'];assert (candidate/'candidate.yaml').read_bytes()==config_bytes
  state.update({'health_http':204,'databases_unchanged':True,'configuration_unchanged':True,'production_mutations':0})
  save('passed')
 except BaseException as error:
  state['failed_phase']=state['phase'];state['error']=str(error);save('failed');raise
 raise SystemExit(0)
if args.stage=='prepare':
 assert not beforefile.exists(),'Existing release record: recover it, do not overwrite'
 acceptance=json.loads((candidate/'full-repair.json').read_text());assert acceptance['entry_diagnostics']['passed'] and acceptance['slot_cleanup']['passed']
 image=(candidate/'image.txt').read_text().strip();source=(candidate/'binary-source.txt').read_text().strip();assert acceptance['source']==source and acceptance['candidate_health']['status']=='passed'
 assert inspect(['cpr-upstream30'])[0]['Image']==inspect([image])[0]['Id']
 run(['podman','image','exists',fallback])
 current=inspect([gateway]+bridges+dbs)
 active=[u for u in units if subprocess.call(['systemctl','--user','is-active','--quiet',u],stdout=subprocess.DEVNULL)==0]
 gateway_dependents(current[0]['Id'])
 before={'containers':current,'active_units':active,'compose':(root/'deploy/compose.yaml').read_text(),'watchdog_active':subprocess.call(['systemctl','--user','is-active','--quiet','cpr-stack-watchdog.timer'])==0}
 beforefile.write_text(json.dumps(before,indent=2)+'\n');state={'image':image,'binary_source':source,'fallback_image':fallback,'db_before':db_identity(current),'production_changed':False};save('prepared');raise SystemExit(0)
assert args.allow_interrupt,'Maintenance approval and --allow-interrupt required'
state=json.loads(statefile.read_text());before=json.loads(beforefile.read_text());definitions=before['containers']
gateway_exists=subprocess.call(['podman','container','exists',gateway])==0
current=inspect(([gateway] if gateway_exists else [])+dbs)
if args.stage=='deploy':
 assert state['phase']=='prepared','Recover existing release state instead of submitting another deploy'
 assert current[0]['Id']==definitions[0]['Id'],'Gateway changed since preparation; coordinate owner'
 image=state['image']
elif args.stage=='resume':
 assert state['phase']=='failed' and gateway_exists,'Resume requires the recorded failed release and restored gateway'
 incident=state['external_recovery_incident'];assert incident.get('owner_handoff'),'External recovery owner must be reconciled in the existing incident first'
 assert current[0]['Id']==incident['current'][0]['id'],'Gateway changed after recovery reconciliation'
 assert db_identity(current)=={x['name']:[x['id'],x['started_at']] for x in incident['current'] if x['name'] in dbs},'Databases changed after recovery reconciliation'
 definitions=inspect([gateway]+bridges+dbs)
 for original in before['containers']:
  if original['Name'] in dbs:
   now=next(x for x in definitions if x['Name']==original['Name'])
   assert now['Mounts']==original['Mounts'],'Database mount differs from original release; do not proceed'
 active=[u for u in units if subprocess.call(['systemctl','--user','is-active','--quiet',u])==0]
 assert set(active)<=set(before['active_units']) and (state.get('resume_boundary') or set(active)==set(before['active_units'])),'Bridge owners changed; reconcile before resume'
 state.setdefault('failed_attempts',[]).append({k:state.get(k) for k in ['phase','failed_phase','last_error','updated_at']})
 state.setdefault('resume_boundary',{'containers':definitions,'active_units':active,'db_before':db_identity(current)})
 image=state['image']
else:
 assert state['phase']!='prepared','Nothing deployed to roll back'
 image=state['fallback_image']
run(['podman','image','exists',image]);before_dbs=db_identity(current)
compose_path=root/'deploy/compose.yaml';compose=compose_path.read_text()
old_image=current[0]['Config']['Image'] if gateway_exists else next((i for i in [state.get('running_image'),state['image'],definitions[0]['Config']['Image'],fallback] if i and i in compose),None)
assert old_image and old_image in compose,'Live Compose image differs; no services changed'
if gateway_exists:gateway_dependents(current[0]['Id'])
try:
 save('stopping_watchdog')
 run(['systemctl','--user','stop','cpr-stack-watchdog.timer','cpr-stack-watchdog.service'])
 save('draining_gateway')
 run(['podman','stop','--ignore','--time','75',gateway]);state['production_changed']=True;save('gateway_stopped')
 # Independent bridges stay alive throughout gateway draining, then stop under their owners.
 if before['active_units']:run(['systemctl','--user','stop']+before['active_units'])
 if args.stage=='deploy':
  save('database_backup')
  with dump.with_suffix('.dump.partial').open('wb') as output:
   run(['podman','exec',dbs[0],'sh','-c','PGPASSWORD="$POSTGRES_PASSWORD" pg_dump -Fc -U "$POSTGRES_USER" -d "$POSTGRES_DB"'],stdout=output)
  dump.with_suffix('.dump.partial').replace(dump);state['backup_bytes']=dump.stat().st_size
 save('removing_gateway_dependents')
 if gateway_exists:
  remaining=gateway_dependents(current[0]['Id'])
  inactive_owned=[x['Name'] for x in remaining if x['Name'] in units.values() and x['State']['Running']]
  assert not inactive_owned,'Systemd bridge still running after owner stop: '+repr(inactive_owned)
  names=[x['Name'] for x in remaining]
  if names:run(['podman','stop','--ignore','--time','10']+names);run(['podman','rm','--ignore']+names)
 run(['podman','rm','--ignore',gateway])
 compose_path.write_text(compose.replace(old_image,image,1))
 for name in ['12_stack_selfheal_watchdog_20260929.sh','26_release_stale_client_slots_20261003.sh']:
  shutil.copyfile(candidate/'source/deploy'/name,root/'deploy'/name)
 save('starting_gateway')
 cmd=[image if x==definitions[0]['Config']['Image'] else x for x in definitions[0]['Config']['CreateCommand']];run(cmd)
 new=inspect([gateway])[0];assert new['Config']['Image']==image
 for original in definitions:
  if original['Name'] not in bridges:continue
  cmd=[x.replace(definitions[0]['Id'],new['Id']) for x in original['Config']['CreateCommand']];run(cmd)
 if before['active_units']:run(['systemctl','--user','start']+before['active_units'])
 save('waiting_for_readiness')
 for _ in range(45):
  try:
   with urllib.request.urlopen('http://127.0.0.1:18082/healthz',timeout=3) as response:
    if response.status==204:break
  except (OSError,urllib.error.URLError):pass
  time.sleep(2)
 else:raise RuntimeError('Gateway readiness failed; preserve state and use compatible rollback if needed')
 state['db_after']=db_identity(inspect(dbs));state['databases_unchanged']=state['db_after']==before_dbs;assert state['databases_unchanged'],'Database identity changed unexpectedly'
 # Now only proven old request members can be released; concurrent new admissions are preserved.
 run(['bash',str(root/'deploy/26_release_stale_client_slots_20261003.sh')])
 if before['watchdog_active']:run(['systemctl','--user','start','cpr-stack-watchdog.timer'])
 state['gateway_id']=new['Id'];state['running_image']=image;save('ready_pending_acceptance')
except BaseException as error:
 state['last_error']=str(error);state['failed_phase']=state['phase'];save('failed');raise
