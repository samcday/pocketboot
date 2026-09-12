from pathlib import Path
import sys,shutil,hashlib,json
sys.path.insert(0,'tools')
import db410c_kexec as k
out=Path(sys.argv[1]);out.mkdir(exist_ok=False)
base=Path('target/kernel/qcom/msm8916-samsung-a5u-eur')
for source,name in [(base/'arch/arm64/boot/Image','kernel.Image'),(base/'pocketpreboot-kernel.img','wrapped.Image'),(base/'boot.img','cold.img'),(base/'pocketboot.dtb','input.dtb'),(base/'.config','kernel.config')]:shutil.copyfile(source,out/name)
k.setprop(out/'input.dtb','/memory@80000000','reg','0','80000000','0','40000000','0','c0000000','0','40000000',kind='x')
k.setprop(out/'input.dtb','/','serial-number','cd0ee037')
k.prepare_external_dtb(out/'input.dtb',out/'external.dtb')
cmd=f'msm.skip_gpu=1 msm.separate_gpu_kms=1 panic=5 oops=panic printk.time=1 pocketboot.log=info pocketboot.drm_page_flips=16 drm.debug=0x6 pocketboot.lab={out.name}'
if len(sys.argv)>2:cmd+=' '+sys.argv[2]
k.package(out/'kernel.Image',out/'external.dtb',cmd,out/'boot.img','mkbootimg')
(out/'cmdline.txt').write_text(cmd+'\n')
(out/'manifest.json').write_text(json.dumps({p.name:{'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in out.iterdir()},indent=2)+'\n')
