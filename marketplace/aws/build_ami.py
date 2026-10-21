#!/usr/bin/env python3
"""Build private AMI in the explicitly authorized account. Records resource IDs."""
import json,os,subprocess,time,tarfile,tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
STATE=Path('/tmp/pine-ami-build')
STATE.mkdir(mode=0o700,exist_ok=True)
ENV={**os.environ,'AWS_CA_BUNDLE':'/etc/ssl/cert.pem','AWS_PAGER':''}
def aws(*args):
    return json.loads(subprocess.check_output(['aws','--profile','marketplace','--region','us-west-2',*args,'--output','json'],env=ENV))
def save(): (STATE/'resources.json').write_text(json.dumps(resources,indent=2))
account=aws('sts','get-caller-identity')['Account']
assert account=='187692046617', 'Wrong AWS account'
if (STATE/'resources.json').exists(): raise SystemExit('Existing build state; inspect before starting another build')
resources={'account':account,'region':'us-west-2','profile':'marketplace'}
name='pine-build-'+str(int(time.time()))
key=STATE/'builder-key'
subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
aws('ec2','import-key-pair','--key-name',name,'--public-key-material','fileb://'+str(key)+'.pub')
resources['key_name']=name;save()
vpc=aws('ec2','describe-vpcs','--filters','Name=is-default,Values=true')['Vpcs'][0]['VpcId']
subnet=aws('ec2','describe-subnets','--filters','Name=vpc-id,Values='+vpc)['Subnets'][0]['SubnetId']
sg=aws('ec2','create-security-group','--group-name',name,'--description','Temporary Pine AMI build SSH','--vpc-id',vpc)['GroupId']
resources['security_group']=sg;save()
ip=subprocess.check_output(['curl','-fsS','https://checkip.amazonaws.com'],text=True).strip()
aws('ec2','authorize-security-group-ingress','--group-id',sg,'--protocol','tcp','--port','22','--cidr',ip+'/32')
images=aws('ec2','describe-images','--owners','099720109477','--filters','Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*','Name=state,Values=available')['Images']
base=max(images,key=lambda i:i['CreationDate'])['ImageId'];resources['base_ami']=base
instance=aws('ec2','run-instances','--image-id',base,'--instance-type','t3.small','--key-name',name,'--subnet-id',subnet,'--security-group-ids',sg,'--associate-public-ip-address','--metadata-options','HttpTokens=required','--block-device-mappings',json.dumps([{'DeviceName':'/dev/sda1','Ebs':{'VolumeSize':16,'VolumeType':'gp3','DeleteOnTermination':True,'Encrypted':False}}]),'--tag-specifications',f'ResourceType=instance,Tags=[{{Key=Name,Value={name}}}]')['Instances'][0]['InstanceId']
resources['builder_instance']=instance;save();print('Builder:',instance,flush=True)
for _ in range(120):
    info=aws('ec2','describe-instances','--instance-ids',instance)['Reservations'][0]['Instances'][0]
    if info['State']['Name']=='running' and info.get('PublicIpAddress'): break
    time.sleep(5)
host=info['PublicIpAddress'];resources['builder_ip']=host;save()
ssh=['ssh','-i',str(key),'-o','StrictHostKeyChecking=accept-new','-o','UserKnownHostsFile='+str(STATE/'known_hosts'),'-o','ConnectTimeout=10','ubuntu@'+host]
for _ in range(60):
    if subprocess.run(ssh+['true'],stderr=subprocess.DEVNULL).returncode==0: break
    time.sleep(5)
else: raise SystemExit('SSH failed; resource IDs saved for cleanup')
archive=STATE/'source.tar.gz'
with tarfile.open(archive,'w:gz') as tar:
    for src,dest in [('server/app.py','app.py'),('server/index.html','index.html'),('server/requirements.txt','requirements.txt'),('deploy/config/config.yaml','config.yaml'),('marketplace/aws/provision.sh','provision.sh')]:tar.add(ROOT/src,arcname=dest)
with archive.open('rb') as f:subprocess.run(ssh+['sudo mkdir -p /opt/pine && sudo tar xz -C /opt/pine'],stdin=f,check=True)
subprocess.run(ssh+['sudo bash /opt/pine/provision.sh'],check=True)
# Remove builder identity and cloud-init history immediately before stopping.
subprocess.run(ssh+["sudo sh -c 'rm -f /root/.ssh/authorized_keys /home/ubuntu/.ssh/authorized_keys /etc/ssh/ssh_host_* /root/.bash_history /home/ubuntu/.bash_history; cloud-init clean --logs --machine-id; sync; shutdown -h now'"],check=True)
for _ in range(120):
    st=aws('ec2','describe-instances','--instance-ids',instance)['Reservations'][0]['Instances'][0]['State']['Name']
    if st=='stopped':break
    time.sleep(5)
else:raise SystemExit('Builder did not stop')
ami=aws('ec2','create-image','--instance-id',instance,'--name',name.replace('build','server'),'--description','Pine single-node server; unique keys generated at first boot; Ubuntu 24.04 x86_64')['ImageId']
resources['ami_id']=ami;save();print('AMI creating:',ami,flush=True)
for _ in range(180):
    image=aws('ec2','describe-images','--image-ids',ami)['Images'][0]
    if image['State']=='available':break
    if image['State']=='failed':raise SystemExit('AMI failed')
    time.sleep(5)
else:raise SystemExit('AMI still pending; inspect saved resources')
subprocess.run(['aws','--profile','marketplace','--region','us-west-2','ec2','modify-image-attribute','--image-id',ami,'--imds-support','Value=v2.0'],env=ENV,check=True)
print('AMI available:',ami,flush=True)
