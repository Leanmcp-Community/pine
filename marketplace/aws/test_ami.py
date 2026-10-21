#!/usr/bin/env python3
"""Launch the built AMI, test isolated synthetic data, then terminate test resources."""
import json,os,subprocess,time
from pathlib import Path
state=Path('/tmp/pine-ami-build')
r=json.loads((state/'resources.json').read_text())
env={**os.environ,'AWS_CA_BUNDLE':'/etc/ssl/cert.pem','AWS_PAGER':''}
def aws(*a):
 output=subprocess.check_output(['aws','--profile','marketplace','--region','us-west-2',*a,'--output','json'],env=env)
 return json.loads(output) if output.strip() else {}
assert aws('sts','get-caller-identity')['Account']=='187692046617'
def save(): (state/'resources.json').write_text(json.dumps(r,indent=2))
instance=aws('ec2','run-instances','--image-id',r['ami_id'],'--instance-type','t3.small','--key-name',r['key_name'],'--security-group-ids',r['security_group'],'--associate-public-ip-address','--metadata-options','HttpTokens=required','--tag-specifications','ResourceType=instance,Tags=[{Key=Name,Value=pine-ami-validation}]')['Instances'][0]['InstanceId']
r['test_instance']=instance;save();print('Test instance:',instance,flush=True)
for _ in range(120):
 info=aws('ec2','describe-instances','--instance-ids',instance)['Reservations'][0]['Instances'][0]
 if info['State']['Name']=='running' and info.get('PublicIpAddress'):break
 time.sleep(5)
ssh=['ssh','-i',str(state/'builder-key'),'-o','StrictHostKeyChecking=accept-new','-o','UserKnownHostsFile='+str(state/'known_hosts'),'-o','ConnectTimeout=10','ubuntu@'+info['PublicIpAddress']]
for _ in range(90):
 if subprocess.run(ssh+['curl -fsS http://127.0.0.1:8080/health'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode==0:break
 time.sleep(5)
else:raise SystemExit('AMI health check failed')
code=r'''
import urllib.request,json,hashlib,os
from pathlib import Path
key=Path('/root/pine-upload-key').read_text().strip()
assert len(key)>=32
headers={'Authorization':'Bearer '+key}
def get(path): return urllib.request.urlopen(urllib.request.Request('http://127.0.0.1:8080'+path,headers=headers)).read()
assert json.loads(get('/v1/summary'))['sessions']==0
assert json.loads(get('/v1/config'))['max_session_bytes']==209715200
assert b'Pine' in urllib.request.urlopen('http://127.0.0.1:8080/').read()
data=b'{"test":"synthetic"}\n'
h={**headers,'X-Device':'01ARZ3NDEKTSV4RRFFQ69G5FAV','X-Session':'01ARZ3NDEKTSV4RRFFQ69G5FAV','X-Path':'traffic.jsonl','X-Offset':'0','X-Source-Length':str(len(data)),'X-SHA256':hashlib.sha256(data).hexdigest()}
for _ in range(2):
 req=urllib.request.Request('http://127.0.0.1:8080/v1/chunks',data=data,headers=h)
 assert json.loads(urllib.request.urlopen(req).read())['stored']
assert json.loads(get('/v1/summary'))['chunks']==1
assert get('/v1/chunks/01ARZ3NDEKTSV4RRFFQ69G5FAV/01ARZ3NDEKTSV4RRFFQ69G5FAV?path=traffic.jsonl')==data
print('PASS: fresh credentials, empty initial database, UI, policy, upload, deduplication, retrieval')
'''
subprocess.run(ssh+['sudo /opt/pine/venv/bin/python -'],input=code,text=True,check=True)
subprocess.run(ssh+['sudo systemctl restart pine'],check=True)
time.sleep(3)
subprocess.run(ssh+['curl -fsS http://127.0.0.1:8080/health'],check=True)
r['validation']='passed: fresh boot, auth upload/retry/retrieval, policy, UI, service restart';save()
aws('ec2','terminate-instances','--instance-ids',instance,r['builder_instance'])
for _ in range(120):
 items=aws('ec2','describe-instances','--instance-ids',instance,r['builder_instance'])['Reservations']
 if all(i['State']['Name']=='terminated' for res in items for i in res['Instances']):break
 time.sleep(5)
aws('ec2','delete-key-pair','--key-name',r['key_name'])
# This API returns no JSON on success.
subprocess.run(['aws','--profile','marketplace','--region','us-west-2','ec2','delete-security-group','--group-id',r['security_group']],env=env,check=True)
r['temporary_resources_cleaned']=True;save()
print('Temporary instances, key pair and security group removed. AMI retained.')
