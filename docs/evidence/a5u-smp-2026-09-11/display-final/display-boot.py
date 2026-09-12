from pathlib import Path
import sys,subprocess,hashlib,json,time
out=Path(sys.argv[1]);manifest=json.loads((out/'manifest.json').read_text())
assert hashlib.sha256((out/'boot.img').read_bytes()).hexdigest()==manifest['boot.img']['sha256']
with (out/'boot.log').open('ab') as log:
 def fb(*args):
  r=subprocess.run(['fastboot','-s','cd0ee037',*map(str,args)],capture_output=True,timeout=35)
  log.write((repr(args)+'\n').encode()+r.stdout+r.stderr);log.flush();r.check_returncode();return r.stdout+r.stderr
 for k,v in [('product','pocketboot'),('serialno','cd0ee037'),('compatible','samsung,a5u-eur')]:assert f'{k}: {v}'.encode() in fb('getvar',k)
 print(fb('boot',out/'boot.img').decode(),flush=True)
 time.sleep(12)
 for k,v in [('product','pocketboot'),('serialno','cd0ee037'),('compatible','samsung,a5u-eur')]:assert f'{k}: {v}'.encode() in fb('getvar',k)
 fb('oem','dmesg');fb('get_staged',out/'dmesg.log')
 s=(out/'dmesg.log').read_text();print('\n'.join(l for l in s.splitlines() if any(w in l for w in ['Linux version','context fault','POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT','Brought up','Kernel command line'])))
