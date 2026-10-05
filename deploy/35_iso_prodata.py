#!/usr/bin/env python3
"""生产数据等价的隔离 CPR 实例（不触碰生产）。

用法（在 ywl 上）:
  python3 /home/zzp/cpr-iso-prodata-20261005/00_setup_iso_instance.py prepare restore up verify
  或单独跑某一阶段: prepare / restore / up / verify

端口: 网关 127.0.0.1:18382   PG 127.0.0.1:18332   Redis 127.0.0.1:18379
镜像: localhost/cpr-ywl:3.19.0-acceptance-5fe6391f（候选，可用 CPR_IMAGE 覆盖）
数据: 生产快照 /home/zzp/codex-proxy-rs/.runtime/34_cutover_db.dump（2026-10-05 13:15, 217158177B, pg custom）
来源: 只读复制 /home/zzp/codex-proxy-rs/deploy/{compose.yaml,config.yaml}，仅改项目名/端口/镜像默认值
验收: verify 打印 healthz、/v1/models 计数、_sqlx_migrations 版本、关键表计数
注: ywl 默认 python3 是 3.6，故不用 subprocess 的 text=/capture_output=
"""
import json, os, re, shutil, subprocess, sys, time, urllib.request
from pathlib import Path

ISO = Path('/home/zzp/cpr-iso-prodata-20261005')
PROD = Path('/home/zzp/codex-proxy-rs')
DUMP = PROD/'.runtime/34_cutover_db.dump'
IMAGE = os.environ.get('CPR_IMAGE', 'localhost/cpr-ywl:3.19.0-acceptance-5fe6391f')
PG = 'cpr-iso-prodata_postgres_1'
COMPOSE = ['/usr/bin/podman', 'compose', '-f', str(ISO/'deploy/compose.yaml')]
CLIENT_KEY = os.environ.get('CPR_ISO_CLIENT_KEY')


def run(cmd, **kw):
    print('+', ' '.join(cmd) if isinstance(cmd, list) else cmd, flush=True)
    return subprocess.run(cmd, check=True, universal_newlines=True, **kw)


def prepare():
    for d in ['deploy', '.runtime/data', '.runtime/logs', '.runtime/postgres', '.runtime/redis']:
        (ISO/d).mkdir(parents=True, exist_ok=True)
    if (ISO/'deploy/compose.yaml').exists():
        print('compose.yaml 已存在，保留（如需重生成先删除）')
    else:
        src = (PROD/'deploy/compose.yaml').read_text()
        out = []
        for line in src.splitlines():
            if line.startswith('name: codex-proxy-rs'):
                line = 'name: cpr-iso-prodata'
            elif "'127.0.0.1:15432:5432'" in line:
                line = line.replace('15432', '18332')
            elif "'127.0.0.1:16379:6379'" in line:
                line = line.replace('16379', '18379')
            elif "'127.0.0.1:18082:8080'" in line:
                line = line.replace('18082', '18382')
            elif "'192.168.233.231:18082:8080'" in line:
                print('  去掉对外端口映射:', line.strip())
                continue
            elif '${CPR_IMAGE:-' in line:
                line = line.replace(line[line.index('${CPR_IMAGE:-'):line.rindex('}') + 1], '${CPR_IMAGE:-%s}' % IMAGE)
            out.append(line)
        (ISO/'deploy/compose.yaml').write_text('\n'.join(out) + '\n')
        shutil.copyfile(PROD/'deploy/config.yaml', ISO/'deploy/config.yaml')
    os.chmod(ISO/'deploy/config.yaml', 0o640)
    run(['/usr/bin/podman', 'unshare', 'chown', '0:10001', str(ISO/'deploy/config.yaml')])
    run(['/usr/bin/podman', 'unshare', 'chown', '-R', '999:999', str(ISO/'.runtime/postgres'), str(ISO/'.runtime/redis')])
    run(['/usr/bin/podman', 'unshare', 'chown', '-R', '10001:10001', str(ISO/'.runtime/data'), str(ISO/'.runtime/logs')])
    print('项目名/端口核对:', [l.strip() for l in (ISO/'deploy/compose.yaml').read_text().splitlines()
                          if l.startswith('name:') or ':18' in l or 'CPR_IMAGE' in l][:8])


def pg_password():
    m = re.search(r"password: &postgres_password '([^']+)'", (ISO/'deploy/config.yaml').read_text())
    if not m:
        sys.exit('无法从 config.yaml 读取 postgres 口令')
    return m.group(1)


def psql(sql, quiet=False):
    cmd = ['/usr/bin/podman', 'exec', '-e', 'PGPASSWORD=' + pg_password(), PG,
           'psql', '-X', '-v', 'ON_ERROR_STOP=1', '-U', 'codex_proxy', '-d', 'codex_proxy', '-Atc', sql]
    if not quiet:
        print('+ isolated psql (credentials omitted)', flush=True)
    return subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True)



def isolate_background_tasks():
    # A production snapshot must not refresh shared OAuth credentials or probe upstreams.
    result = psql("""BEGIN;
WITH changed AS (
 UPDATE provider_accounts SET provider_credentials_json=provider_credentials_json-'refresh_token',
 has_refresh_token=false,next_refresh_at=NULL,credential_revision=credential_revision+1,updated_at=now()
 WHERE has_refresh_token OR provider_credentials_json ? 'refresh_token' RETURNING id)
UPDATE runtime_settings SET account_warmup_enabled=false,account_auto_freeze_probe_enabled=false,
 config_revision=config_revision+1,updated_at=now()
 WHERE account_warmup_enabled OR account_auto_freeze_probe_enabled OR EXISTS(SELECT 1 FROM changed);
UPDATE backup_settings SET schedule_enabled=false,updated_at=now() WHERE id=1 AND schedule_enabled;
COMMIT;""", quiet=True)
    result.check_returncode()


def pg_ready():
    for _ in range(90):
        if psql('select 1', quiet=True).returncode == 0:
            return True
        time.sleep(2)
    return False


def restore():
    if not DUMP.exists():
        sys.exit('缺少生产快照 %s' % DUMP)
    run(COMPOSE + ['up', '-d', 'postgres', 'redis'])
    if not pg_ready():
        sys.exit('PG 未就绪')
    r = psql("select count(*) from information_schema.tables where table_schema='public'")
    if r.returncode != 0:
        sys.exit('psql 失败: ' + r.stderr.strip()[:300])
    n = r.stdout.strip()
    if n != '0':
        print('库里已有 %s 张表，跳过恢复' % n)
        isolate_background_tasks()
        return
    print('恢复 %s (%.0f MB) ...' % (DUMP.name, DUMP.stat().st_size / 1e6), flush=True)
    with DUMP.open('rb') as fh:
        subprocess.run(['/usr/bin/podman', 'exec', '-i', '-e', 'PGPASSWORD=' + pg_password(), PG, 'pg_restore',
                        '-U', 'codex_proxy', '-d', 'codex_proxy', '--no-owner', '--no-privileges', '--exit-on-error'],
                       stdin=fh, check=True)
    isolate_background_tasks()
    print('恢复完成；隔离后台副作用已禁用')



def bridges():
    gateway = 'cpr-iso-prodata_codex-proxy-rs_1'
    gateway_id = json.loads(subprocess.check_output(['podman','inspect',gateway]).decode())[0]['Id']
    image = 'docker.io/alpine/socat@sha256:24220ef2c80a2a421ea08e4624488e985330c421b6aa3329bae14b0933a1d403'
    entries = [('pool',17865,'/home/zzp/.local/share/codex-chatgpt-web/pool'),
               ('full-1',17971,'/home/zzp/.local/share/codex-chatgpt-web/full-1/cpr'),
               ('full-2',17972,'/home/zzp/.local/share/codex-chatgpt-web/full-2/cpr')]
    for suffix, port, source in entries:
        name = 'cpr-iso-prodata-' + suffix + '-bridge'
        if subprocess.call(['podman','container','exists',name]) == 0:
            current = json.loads(subprocess.check_output(['podman','inspect',name]).decode())[0]
            assert current['State']['Running'] and current['HostConfig']['NetworkMode'] == 'container:' + gateway_id, 'isolated bridge must be reconciled after gateway recreation: ' + name
            continue
        run(['podman','run','-d','--name',name,'--network','container:'+gateway_id,
             '--security-opt','label=disable','--security-opt','no-new-privileges:true','--cap-drop','ALL','--read-only',
             '-v',source+':/run/provider:ro',image,
             'TCP-LISTEN:'+str(port)+',fork,reuseaddr,bind=127.0.0.1','UNIX-CONNECT:/run/provider/provider.sock'])


def up():
    isolate_background_tasks()
    run(COMPOSE + ['up', '-d'])
    bridges()


def verify():
    with urllib.request.urlopen('http://127.0.0.1:18382/healthz', timeout=8) as response:
        assert response.status == 204, 'isolated healthz must return 204'
    key = CLIENT_KEY
    if not key:
        result = psql("SELECT key FROM client_api_keys WHERE name='test' AND enabled LIMIT 1", quiet=True)
        result.check_returncode()
        key = result.stdout.strip()
    if not key:
        sys.exit('No enabled test key in isolated DB; set CPR_ISO_CLIENT_KEY privately')
    request = urllib.request.Request('http://127.0.0.1:18382/v1/models', headers={'Authorization': 'Bearer ' + key})
    with urllib.request.urlopen(request, timeout=15) as response:
        models = json.load(response)['data']
    result = psql("""SELECT json_build_object(
      'accounts',(SELECT count(*) FROM provider_accounts),
      'keys',(SELECT count(*) FROM client_api_keys),
      'groups',(SELECT count(*) FROM account_groups),
      'migration',(SELECT max(version) FROM _sqlx_migrations),
      'refreshable_accounts',(SELECT count(*) FROM provider_accounts WHERE has_refresh_token OR provider_credentials_json ? 'refresh_token'),
      'scheduled_backup',(SELECT schedule_enabled FROM backup_settings WHERE id=1),
      'warmup',account_warmup_enabled,'auto_freeze_probe',account_auto_freeze_probe_enabled)
      FROM runtime_settings""", quiet=True)
    result.check_returncode()
    receipt = json.loads(result.stdout)
    assert [receipt[x] for x in ['accounts','keys','groups','migration']] == [43,37,38,22], receipt
    assert receipt['refreshable_accounts'] == 0 and not any(receipt[x] for x in ['scheduled_backup','warmup','auto_freeze_probe']), receipt
    receipt.update({'health_http':204,'test_key_model_count':len(models),'passed':True})
    (ISO/'verification.json').write_text(json.dumps(receipt, indent=2)+'\n')
    print(json.dumps(receipt, ensure_ascii=False))


if __name__ == '__main__':
    os.umask(0o077)
    stages = sys.argv[1:] or ['prepare', 'restore', 'up', 'verify']
    for s in stages:
        print('=== %s ===' % s, flush=True)
        {'prepare': prepare, 'restore': restore, 'up': up, 'bridges': bridges, 'verify': verify}[s]()
