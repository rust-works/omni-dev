"""Run a fresh live all-open baseline: capture.py SUCCINCTLY_CHECKOUT OUTPUT_DIR.
Requires a built target/debug/omni-dev and configured Jev credentials.
"""
import subprocess,os,pathlib,json,datetime,time,gzip,hashlib,sys
wt=pathlib.Path(__file__).resolve().parents[3]
if len(sys.argv)!=3: raise SystemExit('usage: capture.py SUCCINCTLY_CHECKOUT OUTPUT_DIR')
p=pathlib.Path(sys.argv[2]).resolve(); p.mkdir(parents=True,exist_ok=False)
fixture=str(p/'github-responses')
cmd=[str(wt/'target/debug/omni-dev'),'ai','jev','route','--all-open','--refresh','--ladders','anthropic','--jev-model','jev-latest','-o','json','-C',str(pathlib.Path(sys.argv[1]).resolve())]
env=os.environ.copy();env.update(OMNI_DEV_GH_BIN=str(pathlib.Path(__file__).with_name('gh_fixture.py')),ROUTE_GH_FIXTURE_DIR=fixture)
start=datetime.datetime.now(datetime.timezone.utc).isoformat(); clock=time.monotonic()
r=subprocess.run(cmd,cwd=wt,env=env,capture_output=True)
(p/'baseline.stderr.txt').write_bytes(r.stderr)
with gzip.GzipFile(filename=str(p/'baseline.json.gz'),mode='wb',mtime=0) as f:f.write(r.stdout)
records={x.name:json.loads(x.read_text()) for x in pathlib.Path(fixture).glob('*.json')}
with gzip.GzipFile(filename=str(p/'github-responses.json.gz'),mode='wb',mtime=0) as f:f.write(json.dumps(records,indent=2).encode())
metadata={'command':cmd,'started_at':start,'finished_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'seconds':time.monotonic()-clock,'exit_code':r.returncode,'source_commit':subprocess.check_output(['git','-C',str(wt),'rev-parse','HEAD'],text=True).strip(),'version':subprocess.check_output([str(wt/'target/debug/omni-dev'),'--version'],text=True).strip(),'binary_sha256':hashlib.sha256((wt/'target/debug/omni-dev').read_bytes()).hexdigest(),'fixture_environment':{'OMNI_DEV_GH_BIN':'$WT/docs/evaluations/jev-route-2052/gh_fixture.py','ROUTE_GH_FIXTURE_DIR':fixture},'requested_model':'jev-latest','monetary_cost':None,'monetary_cost_note':'Billing receipt and dated model rate not exposed by CLI. Tokens are reported; dollars unknown.'}
(p/'run.json').write_text(json.dumps(metadata,indent=2)+'\n')
print(json.dumps(metadata,indent=2))
