from pathlib import Path
import subprocess,time,json,hashlib,re
r=Path(__file__).resolve().parent
art=r/'kexec';manifest=json.loads((art/'manifest.json').read_text())['artifacts']
script=r/'display-check.sh'
script.write_text('set -eu\ncat /proc/cmdline\ncat /sys/devices/system/cpu/online\ncat /proc/interrupts\ncat /sys/kernel/debug/dri/0/fb\ncat /sys/kernel/debug/dri/0/state\n')
results=[]
with (r/'handoffs.log').open('ab') as log:
 def fb(*args):
  x=subprocess.run(['fastboot','-s','cd0ee037',*map(str,args)],capture_output=True,timeout=35)
  log.write((repr(args)+'\n').encode()+x.stdout+x.stderr);log.flush();x.check_returncode();return x.stdout+x.stderr
 def identify():
  for k,v in [('product','pocketboot'),('serialno','cd0ee037'),('compatible','samsung,a5u-eur')]:assert f'{k}: {v}'.encode() in fb('getvar',k)
 for generation in range(4):
  identify()
  marker='display-final'+(f'-{generation}' if generation else '')
  if generation:
   image=art/f'generation-{generation}.img'
   assert hashlib.sha256(image.read_bytes()).hexdigest()==manifest[image.name]['sha256']
   print(fb('boot',image).decode(),flush=True);time.sleep(10);identify()
  out=r/f'proof/generation-{generation}'
  subprocess.run(['python3','tools/db410c_check_phase.py','--serial','cd0ee037','--compatible','samsung,a5u-eur','--artifacts',str(art),'--output',str(out),'--marker',marker],check=True)
  fb('stage',script);fb('oem','shell-staged');fb('get_staged',out/'display.log')
  fb('oem','dmesg');fb('get_staged',out/'dmesg.log')
  display=(out/'display.log').read_text();dmesg=(out/'dmesg.log').read_text()
  faultlines=[l for l in display.splitlines() if 'qcom-iommu-fault' in l];assert len(faultlines)==1
  irqcounts=list(map(int,faultlines[0].split()[1:5]));assert irqcounts==[0,0,0,0],irqcounts
  assert f'pocketboot.lab={marker}' in display.split()
  assert 'POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested=16 completed=16' in dmesg
  assert not re.search('Unhandled context fault|Kernel panic|Internal error: Oops|WARNING:',dmesg)
  errors=[l for l in dmesg.splitlines() if 'mdp5_irq_error_handler' in l]
  assert len(errors)<=1,errors
  result={'phase':generation,'marker':marker,'four_core_workloads':'PASS','coherency':'PASS','iommu_fault_counts':irqcounts,'page_flips':16,'mdp_error_events':len(errors),'preboot_reentry':generation==3}
  results.append(result);(r/'handoff-results.json').write_text(json.dumps(results,indent=2)+'\n')
  print(f'DISPLAY_SMP_HANDOFF_PASS generation={generation}',flush=True)
