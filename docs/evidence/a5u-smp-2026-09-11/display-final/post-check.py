from pathlib import Path
import subprocess
r=Path(__file__).resolve().parent
with (r/'post-check-fastboot.log').open('ab') as log:
 def fb(*args):
  x=subprocess.run(['fastboot','-s','cd0ee037',*map(str,args)],capture_output=True,timeout=35)
  log.write((repr(args)+'\n').encode()+x.stdout+x.stderr);log.flush();x.check_returncode();return x.stdout+x.stderr
 for k,v in [('product','pocketboot'),('serialno','cd0ee037'),('compatible','samsung,a5u-eur')]:assert f'{k}: {v}'.encode() in fb('getvar',k)
 fb('stage',r/'post-check.sh');fb('oem','shell-staged');fb('get_staged',r/'post-check.log')
 fb('oem','cat:/tmp/pb-pstore/console-ramoops-0');fb('get_staged',r/'previous-console.log')
 fb('oem','dmesg');fb('get_staged',r/'final-dmesg.log')
 print((r/'post-check.log').read_text()[:1800])
