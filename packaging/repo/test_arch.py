#!/usr/bin/env python3
"""Exercise routing/migration and release provenance without touching system config or keys."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def executable(path, content):
    path.write_text('#!/bin/bash\nset -eu\n' + content)
    path.chmod(0o755)


class InstallerTests(unittest.TestCase):
    def run_installer(self, arch):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'bin'
            binary.mkdir()
            etc = root / 'etc'
            (etc / 'pacman.d').mkdir(parents=True)
            (etc / 'pacman.conf').write_text('Include = /etc/pacman.d/cloudmail.conf\n[other]\n')
            (etc / 'pacman.d/cloudmail.conf').write_text('old x86 config')
            home = root / 'home'
            hook = home / '.config/omarchy/hooks/pre-refresh-pacman.d'
            hook.mkdir(parents=True)
            (hook / 'cloudmail').write_text('old hook')
            executable(binary / 'sudo', 'exec "$@"\n')
            executable(binary / 'chown', 'exit 0\n')
            executable(binary / 'install', 'while [[ $# -gt 0 ]]; do case "$1" in -d) shift;; -o|-g) shift 2;; *) mkdir -p "$1"; shift;; esac; done\n')
            executable(binary / 'uname', f'echo {arch}\n')
            executable(binary / 'curl', 'while [[ $1 != -o ]]; do shift; done; echo public-key > "$2"\n')
            executable(binary / 'gpg', "echo 'fpr:::::::::35C47A06567940B6796B4D0F9B3C7BDF85268B31:'\n")
            executable(binary / 'getent', f"echo 'root:x:0:0::{home}:/bin/bash'\n")
            for command in ('pacman-key', 'pacman', 'omarchy-pkg-add'):
                executable(binary / command, 'echo "$0 $*" >> "$CALL_LOG"\n')
            source = (ROOT / 'packaging/repo/install.sh').read_text().replace('/etc/', f'{etc}/')
            # Existing include must use the same fixture path as the transformed installer.
            (etc / 'pacman.conf').write_text(f'Include = {etc}/pacman.d/cloudmail.conf\n[other]\n')
            script = root / 'install.sh'
            script.write_text(source)
            log = root / 'calls'
            env = dict(os.environ, PATH=f'{binary}:{os.environ["PATH"]}', CALL_LOG=str(log), USER='root', SUDO_USER='root')
            result = subprocess.run(['bash', str(script)], env=env, capture_output=True, text=True)
            if arch not in ('x86_64', 'aarch64'):
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(log.exists())
                self.assertTrue((etc / 'pacman.d/cloudmail.conf').exists())
                return
            self.assertEqual(result.returncode, 0, result.stderr)
            repo = 'cloudmail-aarch64' if arch == 'aarch64' else 'cloudmail'
            self.assertIn(f'[{repo}]', (etc / f'pacman.d/{repo}.conf').read_text())
            self.assertIn('cloudmail npm', log.read_text())
            self.assertTrue((hook / repo).exists())
            if arch == 'aarch64':
                self.assertFalse((hook / 'cloudmail').exists())
                self.assertFalse((etc / 'pacman.d/cloudmail.conf').exists())
                self.assertNotIn(f'Include = {etc}/pacman.d/cloudmail.conf\n', (etc / 'pacman.conf').read_text())

    def test_x86(self): self.run_installer('x86_64')
    def test_arm_migration(self): self.run_installer('aarch64')
    def test_unsupported(self): self.run_installer('riscv64')


class VerificationTests(unittest.TestCase):
    def test_exact_release_and_native_arm_image(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable(root / 'uname', 'echo aarch64\n')
            executable(root / 'docker', 'printf "%s\\n" "$@" > "$CALL_LOG"\n')
            log = root / 'args'
            env = dict(os.environ, PATH=f'{root}:{os.environ["PATH"]}', CALL_LOG=str(log), ARM_BUILD_IMAGE='test-arm:latest')
            subprocess.run(['bash', str(ROOT / 'bin/verify-release'), '0.4.2', 'aarch64'], env=env, check=True)
            args = log.read_text()
            self.assertIn('CLOUDMAIL_RELEASES_URL=https://github.com/ferdousbhai/cloud-mail/releases/download/v0.4.2', args)
            self.assertNotIn('releases/latest', args)
            self.assertIn('test-arm:latest', args)
            self.assertIn('linux/arm64', args)
            log.unlink()
            result = subprocess.run(['bash', str(ROOT / 'bin/verify-release'), '0.4.2', 'x86_64'], env=env, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(log.exists())


class ReleaseTests(unittest.TestCase):
    def run_release(self, failure=''):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'bin').mkdir()
            (root / 'packaging/repo').mkdir(parents=True)
            (root / 'bin/release').write_text((ROOT / 'bin/release').read_text())
            (root / 'Cargo.toml').write_text('version = "0.4.2"\n')
            (root / 'packaging/repo/install.sh').write_text('installer fixture\n')
            for args in (['init', '-q'], ['add', '.'], ['-c', 'user.name=Test', '-c', 'user.email=test@example.com', 'commit', '-qm', 'fixture'], ['tag', 'v0.4.2']):
                subprocess.run(['git', *args], cwd=root, check=True)
            commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
            binary = root / 'mocks'
            binary.mkdir()
            log = root / 'calls'
            executable(binary / 'gh', '''
echo "gh $*" >> "$CALL_LOG"
case "$1 $2" in
  'api '*) echo "$COMMIT" ;;
  'run list') echo 123 ;;
  'run watch') [[ $FAILURE != ci ]] ;;
  'run view') printf '%s\\tpush\\tsuccess\\tNative packages\\n' "$COMMIT" ;;
  'run download')
    arch=x86_64
    [[ "$*" != *cloudmail-aarch64* ]] || arch=aarch64
    dest=${!#}; mkdir -p "$dest/meta"
    provenance=$COMMIT
    [[ $FAILURE != source || $arch != aarch64 ]] || provenance=wrong
    echo "$provenance" > "$dest/source-commit.txt"
    metadata_arch=$arch
    [[ $FAILURE != architecture || $arch != aarch64 ]] || metadata_arch=x86_64
    printf 'pkgname = cloudmail\\npkgver = 0.4.2-1\\narch = %s\\n' "$metadata_arch" > "$dest/meta/.PKGINFO"
    tar -C "$dest/meta" -czf "$dest/cloudmail-0.4.2-1-$arch.pkg.tar.zst" .PKGINFO ;;
  'release view') echo true ;;
esac
''')
            executable(binary / 'bsdtar', 'exec tar "$@"\n')
            executable(binary / 'gpg', '''
echo "gpg $*" >> "$CALL_LOG"
if [[ "$*" == *--detach-sign* ]]; then
  [[ $FAILURE != signing ]] || exit 1
  touch "${!#}.sig"
elif [[ "$*" == *--export* ]]; then echo public-key; fi
''')
            executable(binary / 'repo-add', '''
repo=${@: -2:1}
repo=${repo%.db.tar.gz}
for suffix in db.tar.gz db.tar.gz.sig files.tar.gz files.tar.gz.sig; do echo database > "$repo.$suffix"; done
for suffix in db db.sig files files.sig; do
  target=${suffix/.sig/}.tar.gz
  [[ $suffix != *.sig ]] || target+=.sig
  ln -s "$repo.$target" "$repo.$suffix"
done
''')
            env = dict(os.environ, PATH=f'{binary}:{os.environ["PATH"]}', CALL_LOG=str(log), COMMIT=commit, FAILURE=failure)
            result = subprocess.run(['bash', 'bin/release', '0.4.2'], cwd=root, env=env, capture_output=True, text=True)
            calls = log.read_text()
            if failure:
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('gh release upload', calls)
                if failure in ('source', 'architecture', 'ci'):
                    self.assertNotIn('--detach-sign', calls)
            else:
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                assets = next(root.glob('target/release-repo-*/assets'))
                for repo in ('cloudmail', 'cloudmail-aarch64'):
                    self.assertTrue((assets / f'{repo}.db').is_file())
                    self.assertFalse((assets / f'{repo}.db').is_symlink())
                    self.assertTrue((assets / f'{repo}-signing-key.asc').exists())
                self.assertIn('gh release edit v0.4.2', calls)
                self.assertEqual((assets / 'install.sh').read_text(), 'installer fixture\n')

    def test_success(self): self.run_release()
    def test_wrong_source(self): self.run_release('source')
    def test_wrong_architecture(self): self.run_release('architecture')
    def test_ci_failure(self): self.run_release('ci')
    def test_signing_failure(self): self.run_release('signing')


if __name__ == '__main__':
    unittest.main()
