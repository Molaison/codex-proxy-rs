#!/usr/bin/env python3
"""Production and the previous18140 rehearsal are frozen. The only allowed command is:
python3 ~/codex-proxy-rs/.runtime/30_upstream_acceptance/source/deploy/34_integrated_release.py upgrade-isolated
Wait for the model-catalog owner to release18382 first. Uses candidate/image.txt.
Recreates only cpr-iso-prodata gateway and its six bridges, never PG/Redis or Web.
Receipt/log: .runtime/30_upstream_acceptance/iso-release.{json,log}.
Full browser/provider sockets remain shared: no Full generation qualification.
"""
import argparse,datetime,fcntl,json,os,subprocess,time,urllib.request,urllib.error
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('stage',choices=['upgrade-isolated','rehearse','prepare','deploy','resume','rollback']);p.add_argument('--allow-interrupt',action='store_true');args=p.parse_args();os.umask(0o077)
if args.stage!='upgrade-isolated':p.error('PRODUCTION_FROZEN: only upgrade-isolated on18382 is permitted; production and18140 operations remain disabled')
root=Path('/home/zzp/codex-proxy-rs');rt=root/'.runtime';candidate=rt/'30_upstream_acceptance'
if args.stage=='upgrade-isolated':
 gateway='cpr-iso-prodata_codex-proxy-rs_1';units={}
 bridges=['cpr-iso-prodata_cpa-bridge_1','cpr-iso-prodata_sub2api-bridge_1','cpr-iso-prodata_chatgpt-web-bridge_1','cpr-iso-prodata-pool-bridge','cpr-iso-prodata-full-1-bridge','cpr-iso-prodata-full-2-bridge']
 dbs=['cpr-iso-prodata_postgres_1','cpr-iso-prodata_redis_1'];statefile=candidate/'iso-release.json'
log=(candidate/'iso-release.log').open('a')
lock=(candidate/'isolated-maintenance.lock').open('a');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
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
if args.stage=='upgrade-isolated':
 assert not statefile.exists(),'Existing isolated release: recover its state, do not resubmit'
 definitions=inspect([gateway]+bridges+dbs);old=definitions[0]
 image=(candidate/'image.txt').read_text().strip();run(['podman','image','exists',image])
 assert old['State']['Running'] and old['Config']['Image']!=image,'Expected a running isolated gateway and a new candidate image'
 gateway_dependents(old['Id'])
 iso=Path('/home/zzp/cpr-iso-prodata-20261005');compose_path=iso/'deploy/compose.yaml';compose=compose_path.read_text()
 assert old['Config']['Image'] in compose,'Isolated compose differs; no services changed'
 config=(iso/'deploy/config.yaml').read_bytes()
 state={'target':'http://127.0.0.1:18382','image':image,'db_before':db_identity(definitions),'containers_before':definitions,'compose_before':compose}
 save('prepared')
 try:
  save('draining_isolated_gateway');run(['podman','stop','--time','75',gateway])
  remaining=gateway_dependents(old['Id']);names=[x['Name'] for x in remaining]
  run(['podman','stop','--ignore','--time','5']+names);run(['podman','rm','--ignore']+names)
  run(['podman','rm',gateway])
  save('starting_isolated_gateway')
  run([image if x==old['Config']['Image'] else x for x in old['Config']['CreateCommand']])
  new=inspect([gateway])[0]
  for original in definitions:
   if original['Name'] in bridges:
    run([x.replace(old['Id'],new['Id']) for x in original['Config']['CreateCommand']])
  for _ in range(45):
   try:
    with urllib.request.urlopen('http://127.0.0.1:18382/healthz',timeout=3) as response:
     if response.status==204:break
   except (OSError,urllib.error.URLError):pass
   time.sleep(2)
  else:raise RuntimeError('Isolated readiness failed; recover this record, never production')
  assert new['Config']['Image']==image
  state['db_after']=db_identity(inspect(dbs));assert state['db_after']==state['db_before']
  assert (iso/'deploy/config.yaml').read_bytes()==config
  compose_path.write_text(compose.replace(old['Config']['Image'],image,1))
  state.update({'gateway_id':new['Id'],'health_http':204,'databases_unchanged':True,'config_unchanged':True,'production_mutations':0})
  save('ready_pending_acceptance')
 except BaseException as error:
  state['failed_phase']=state['phase'];state['error']=str(error);save('failed');raise
 raise SystemExit(0)
