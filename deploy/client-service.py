#!/usr/bin/env python3
"""Install/remove a per-user uploader service. Configure the CLI first."""
import argparse
import os
import plistlib
import shutil
import subprocess
import sys
from pathlib import Path
p=argparse.ArgumentParser()
p.add_argument('action', choices=['install','uninstall'])
a=p.parse_args()
home=Path(os.environ.get('PINE_HOME',str(Path.home()/'.pine'))).resolve()
binary=shutil.which('pine')
if a.action=='install' and (not binary or not (home/'cloud.json').exists()):
    p.error('Install pine and run cloud configure first')
if sys.platform=='darwin':
    path=Path.home()/'Library/LaunchAgents/com.pine.sync.plist'
    domain=f'gui/{os.getuid()}'
    subprocess.run(['launchctl','bootout',domain+'/com.pine.sync'],check=False,stderr=subprocess.DEVNULL)
    if a.action=='install':
        path.parent.mkdir(parents=True,exist_ok=True)
        path.write_bytes(plistlib.dumps({'Label':'com.pine.sync','ProgramArguments':[binary,'cloud','sync'],'EnvironmentVariables':{'PINE_HOME':str(home)},'RunAtLoad':True,'KeepAlive':{'SuccessfulExit':False},'ThrottleInterval':30}))
        subprocess.run(['launchctl','bootstrap',domain,str(path)],check=True)
    else: path.unlink(missing_ok=True)
elif sys.platform.startswith('linux'):
    path=Path.home()/'.config/systemd/user/pine-sync.service'
    subprocess.run(['systemctl','--user','disable','--now','pine-sync'],check=False,stderr=subprocess.DEVNULL)
    if a.action=='install':
        def quote(s): return '"'+s.replace('\\','\\\\').replace('"','\\"').replace('%','%%')+'"'
        path.parent.mkdir(parents=True,exist_ok=True)
        path.write_text('[Unit]\nDescription=Pine cloud uploader\n[Service]\nExecStart='+quote(binary)+' cloud sync\nEnvironment='+quote('PINE_HOME='+str(home))+'\nRestart=on-failure\nRestartSec=30\n[Install]\nWantedBy=default.target\n')
        subprocess.run(['systemctl','--user','daemon-reload'],check=True)
        subprocess.run(['systemctl','--user','enable','--now','pine-sync'],check=True)
    else:
        path.unlink(missing_ok=True)
        subprocess.run(['systemctl','--user','daemon-reload'],check=True)
else: p.error('Automatic service installation supports macOS and Linux')
