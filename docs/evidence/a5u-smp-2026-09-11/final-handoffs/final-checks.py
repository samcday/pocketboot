from pathlib import Path
import subprocess,time,hashlib,json,random
root=Path(__file__).resolve().parent
out=root/'final-checks';out.mkdir(exist_ok=False)
log=(out/'fastboot.log').open('ab')
def fb(*args,check=True):
    log.write((repr(args)+'\n').encode());log.flush()
    r=subprocess.run(['fastboot','-s','cd0ee037',*map(str,args)],capture_output=True,timeout=35)
    log.write(r.stdout+r.stderr);log.flush()
    if check:r.check_returncode()
    return r.stdout+r.stderr
for key,value in [('product','pocketboot'),('serialno','cd0ee037'),('compatible','samsung,a5u-eur')]:assert f'{key}: {value}'.encode() in fb('getvar',key)
records=[]
for size in [1,511,512,513,4095,4096,4097,8193,16383,16384,16385,32769,41054,65535,65536,65537,1048593]:
    data=random.Random(size).randbytes(size)
    source=out/'upload.bin';dest=out/'readback.bin';source.write_bytes(data)
    fb('stage',source);fb('get_staged',dest)
    assert dest.read_bytes()==data,(size,'readback mismatch')
    records.append({'bytes':size,'sha256':hashlib.sha256(data).hexdigest(),'result':'PASS'})
    print(f'USB_ROUNDTRIP_PASS bytes={size}',flush=True)
(out/'usb-boundaries.json').write_text(json.dumps(records,indent=2)+'\n')
# Mounting pstore exposes the previous kernel's console copied at probe.
script=out/'pstore.sh';script.write_text('mkdir -p /tmp/pb-pstore\nmount -t pstore pstore /tmp/pb-pstore\nls -l /tmp/pb-pstore\n')
fb('stage',script);fb('oem','shell-staged',check=False);fb('get_staged',out/'pstore-files.log')
fb('oem','cat:/tmp/pb-pstore/console-ramoops-0',check=False);fb('get_staged',out/'previous-console.log',check=False)
start=time.monotonic()
for round_no in range(21):
    script=out/'work.sh'
    script.write_text('set -eu\ncat /proc/cmdline\ncat /sys/devices/system/cpu/online\n/tmp/pb-smp-v2/work\n/tmp/pb-coherency-v1/work\n')
    fb('stage',script);fb('oem','shell-staged');fb('get_staged',out/f'round-{round_no:02d}.log')
    report=(out/f'round-{round_no:02d}.log').read_text()
    assert 'pocketboot.lab=kexec-generation-3-preboot-configured' in report
    assert 'SMP_WORK_PASS all_four_cpus' in report
    assert 'SMP_COHERENCY_PASS all_four_cpus_shared_memory_and_migration' in report
    print(f'SOAK_PASS round={round_no} elapsed={time.monotonic()-start:.1f}',flush=True)
    if round_no!=20:time.sleep(max(0,start+(round_no+1)*15-time.monotonic()))
fb('oem','dmesg');fb('get_staged',out/'final-dmesg.log')
(out/'soak.json').write_text(json.dumps({'rounds':21,'elapsed_seconds':time.monotonic()-start,'result':'PASS'},indent=2)+'\n')
print('FINAL_STABILITY_PASS',flush=True)
