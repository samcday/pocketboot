from pathlib import Path
import subprocess,time,json,hashlib
root=Path(__file__).resolve().parent
manifest=json.loads((root/'kexec/manifest.json').read_text())['artifacts']
for generation,image,marker in [(1,'generation-1.img','kexec-generation-1'),(2,'generation-2.img','kexec-generation-2'),(3,'generation-3-preboot-configured.img','kexec-generation-3-preboot-configured')]:
    destination=root/'kexec'/image
    assert hashlib.sha256(destination.read_bytes()).hexdigest()==manifest[image]['sha256']
    with (root/'handoffs.log').open('ab') as log:
        def fb(*args,timeout=40):
            log.write((repr(args)+'\n').encode());log.flush()
            r=subprocess.run(['fastboot','-s','cd0ee037',*map(str,args)],capture_output=True,timeout=timeout)
            log.write(r.stdout+r.stderr);log.flush();r.check_returncode();return r.stdout+r.stderr
        for key,value in [('product','pocketboot'),('serialno','cd0ee037'),('compatible','samsung,a5u-eur')]:
            assert f'{key}: {value}'.encode() in fb('getvar',key)
        print(f'GENERATION {generation}: booting {image}',flush=True)
        print(fb('boot',destination).decode(),flush=True)
        time.sleep(8)
        product=b''
        for attempt in range(10):
            try:product=fb('getvar','product',timeout=3);break
            except subprocess.TimeoutExpired:time.sleep(1)
        assert b'product: pocketboot' in product,product
    subprocess.run(['python3','tools/db410c_check_phase.py','--serial','cd0ee037','--compatible','samsung,a5u-eur','--artifacts',str(root/'kexec'),'--output',str(root/f'proof/generation-{generation}'),'--marker',marker],check=True)
    print(f'GENERATION {generation}: ALL CHECKS PASSED',flush=True)
