from pathlib import Path
import subprocess,sys
out=Path(sys.argv[1])
with (out/'deep-capture.log').open('ab') as log:
 def fb(*args):
  r=subprocess.run(['fastboot','-s','cd0ee037',*map(str,args)],capture_output=True,timeout=35)
  log.write((repr(args)+'\n').encode()+r.stdout+r.stderr);log.flush();r.check_returncode();return r.stdout+r.stderr
 for k,v in [('product','pocketboot'),('serialno','cd0ee037'),('compatible','samsung,a5u-eur')]:
  assert f'{k}: {v}'.encode() in fb('getvar',k)
 fb('stage',out/'deep-inventory.sh');fb('oem','shell-staged');fb('get_staged',out/'deep-inventory.log')
 fb('oem','dmesg');fb('get_staged',out/'later-dmesg.log')
 print((out/'deep-inventory.log').read_text()[:5500])
