"""Build APK tanpa Gradle: aapt2 -> javac -> d8 -> zipalign -> apksigner.

  python build.py            # build
  python build.py --install  # build lalu adb install

Butuh: JDK di PATH, Android SDK di %LOCALAPPDATA%\\Android\\Sdk.
"""

import os
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE / "build"
SDK = Path(os.environ.get("ANDROID_HOME") or Path(os.environ["LOCALAPPDATA"]) / "Android" / "Sdk")


def newest(d):
    return sorted((p for p in d.iterdir() if p.is_dir()), key=lambda p: p.name)[-1]


BT = newest(SDK / "build-tools")
# platform stabil saja (android-36), hindari preview seperti android-37.0
_stable = [p for p in (SDK / "platforms").iterdir() if p.name.rpartition("-")[2].isdigit()]
ANDROID_JAR = max(_stable, key=lambda p: int(p.name.rpartition("-")[2])) / "android.jar"
KEYSTORE = OUT / "debug.keystore"


def run(*cmd, **kw):
    cmd = [str(c) for c in cmd]
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if r.returncode:
        print("GAGAL:", " ".join(cmd))
        print(r.stdout, r.stderr)
        sys.exit(1)
    return r.stdout


def main():
    print(f"build-tools {BT.name}, {ANDROID_JAR.parent.name}")
    if OUT.exists():
        shutil.rmtree(OUT)
    (OUT / "classes").mkdir(parents=True)

    # 1. resources
    run(BT / "aapt2.exe", "compile", "--dir", HERE / "res", "-o", OUT / "res.zip")
    run(BT / "aapt2.exe", "link",
        "-o", OUT / "base.apk",
        "-I", ANDROID_JAR,
        "--manifest", HERE / "AndroidManifest.xml",
        "-R", OUT / "res.zip",
        "--java", OUT / "gen",
        "--min-sdk-version", "24",
        "--target-sdk-version", "34")

    # 2. java -> class
    sources = [str(p) for p in (HERE / "java").rglob("*.java")]
    sources += [str(p) for p in (OUT / "gen").rglob("*.java")]
    run("javac", "--release", "11", "-nowarn",
        "-classpath", ANDROID_JAR,
        "-d", OUT / "classes", *sources)

    # 3. class -> dex
    classes = [str(p) for p in (OUT / "classes").rglob("*.class")]
    run(BT / "d8.bat", "--lib", ANDROID_JAR, "--min-api", "24",
        "--output", OUT, *classes)

    # 4. dex masuk apk
    shutil.copy(OUT / "base.apk", OUT / "unsigned.apk")
    with zipfile.ZipFile(OUT / "unsigned.apk", "a", zipfile.ZIP_DEFLATED) as z:
        z.write(OUT / "classes.dex", "classes.dex")

    # 5. keystore debug sekali pakai
    if not KEYSTORE.exists():
        run("keytool", "-genkeypair", "-keystore", KEYSTORE, "-storepass", "android",
            "-keypass", "android", "-alias", "debug", "-keyalg", "RSA", "-keysize", "2048",
            "-validity", "10000", "-dname", "CN=Zerofuss")

    # 6. align + sign
    run(BT / "zipalign.exe", "-f", "4", OUT / "unsigned.apk", OUT / "zerofuss-bridge.apk")
    run(BT / "apksigner.bat", "sign", "--ks", KEYSTORE, "--ks-pass", "pass:android",
        "--key-pass", "pass:android", OUT / "zerofuss-bridge.apk")

    apk = OUT / "zerofuss-bridge.apk"
    print(f"OK -> {apk}  ({apk.stat().st_size/1024:.0f} KB)")

    if "--install" in sys.argv:
        adb = SDK / "platform-tools" / "adb.exe"
        print(run(adb, "install", "-r", apk))


if __name__ == "__main__":
    main()
