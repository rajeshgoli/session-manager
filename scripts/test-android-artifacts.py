import os, pathlib, subprocess, time
sdk=pathlib.Path('/opt/homebrew/share/android-commandlinetools')
adb=str(sdk/'platform-tools/adb')
serial='emulator-5580'
out=pathlib.Path(os.environ.get('SHOTS_DIR','/tmp/2224-shots'));out.mkdir(exist_ok=True)
assert serial not in subprocess.check_output([adb,'devices'],text=True), 'Preview emulator port already in use'
log=open(out/'emulator.log','w')
p=subprocess.Popen([str(sdk/'emulator/emulator'),'-avd','sm_mtls_api35','-port','5580','-read-only','-no-window','-no-audio','-no-snapshot','-no-boot-anim','-memory','2048'],stdout=log,stderr=subprocess.STDOUT)
def run(*args,**kwargs):
    return subprocess.run([adb,'-s',serial,*args],check=True,timeout=90,**kwargs)
try:
    for _ in range(90):
        result=subprocess.run([adb,'-s',serial,'shell','getprop','sys.boot_completed'],capture_output=True,text=True,timeout=5)
        if result.stdout.strip()=='1': break
        if p.poll() is not None: raise RuntimeError('Emulator exited; inspect emulator.log')
        time.sleep(2)
    else: raise RuntimeError('Emulator boot timed out')
    run('install','-r','-d','android-app/app/build/outputs/apk/debug/app-debug.apk')
    run('shell','am','start','-W','-n','li.rajeshgo.sm/.debug.ArtifactsPreviewActivity','--ez','markdown',str(os.environ.get('MARKDOWN')=='1').lower(),'--ez','backup',str(os.environ.get('BACKUP')=='1').lower())
    time.sleep(3)
    with open(out/'android-artifacts.png','wb') as f: run('exec-out','screencap','-p',stdout=f)
    run('shell','uiautomator','dump','/sdcard/2224-ui.xml')
    run('pull','/sdcard/2224-ui.xml',str(out/'android-ui.xml'))
    content=(out/'android-ui.xml').read_text()
    assert ('Retention' in content if os.environ.get('MARKDOWN')=='1' else 'TAKING UP SPACE' in content and '1.7 TB of 4.0 TB used' in content), content[:2000]
    if os.environ.get('BACKUP')=='1':
        assert 'Drive health: Failing' in content and '2.9 TB used' in content and 'Holding shortened for space on 2026-10-13' in content, content[:2000]
    assert 'Needs you' not in content
    print('Native overview rendered: rounded storage, largest folders, no Needs you badge')
finally:
    subprocess.run([adb,'-s',serial,'emu','kill'],capture_output=True,timeout=10)
    try:p.wait(timeout=20)
    except subprocess.TimeoutExpired:p.kill();p.wait()
    log.close()
