"""Build a pinned native broker image without inference or copying an owner profile."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

from turnrun import outside_repo, sha256_file
from turnrun_isolation import IMAGE

BASE_URL = 'https://downloads.claude.ai/claude-code-releases'


def build(*, version, platform, base, work):
    if not re.fullmatch(r'\d+\.\d+\.\d+', version) or platform not in ('linux-arm64', 'linux-x64'):
        raise ValueError('pin a numeric CLI version and supported Linux platform')
    if not IMAGE.fullmatch(base):
        raise ValueError('pin the base image by immutable image ID or digest')
    if base.startswith('sha256:'):
        metadata = json.loads(subprocess.check_output(['docker', 'image', 'inspect', base]))[0]
        digests = metadata.get('RepoDigests') or []
        if not digests or not IMAGE.fullmatch(digests[0]):
            raise ValueError('the local base image needs a repository digest for BuildKit')
        base = digests[0]
    work = outside_repo(work, 'native image build context')
    work.mkdir(parents=True, exist_ok=False)
    manifest_path = work / 'manifest.json'
    subprocess.run(['curl', '--fail', '--silent', '--show-error', '--location', '--max-time', '60',
                    f'{BASE_URL}/{version}/manifest.json', '--output', str(manifest_path)], check=True)
    manifest = json.loads(manifest_path.read_text())
    selected = manifest['platforms'][platform]
    checksum = selected['checksum']
    if manifest['version'] != version or not re.fullmatch('[0-9a-f]{64}', checksum):
        raise ValueError('invalid publisher manifest')
    binary = work / 'claude'
    subprocess.run(['curl', '--fail', '--silent', '--show-error', '--location', '--max-time', '300',
                    f'{BASE_URL}/{version}/{platform}/claude', '--output', str(binary)], check=True)
    if sha256_file(binary) != checksum or binary.stat().st_size != selected['size']:
        raise ValueError('publisher binary checksum/size mismatch')
    binary.chmod(0o755)
    bridge = Path(__file__).parent / 'turnrun_native_bridge.py'
    shutil.copyfile(bridge, work / bridge.name)
    # Only these three files are sent to Docker. Native profiles and evaluation data stay out.
    (work / '.dockerignore').write_text('*\n!Dockerfile\n!claude\n!turnrun_native_bridge.py\n')
    (work / 'Dockerfile').write_text(
        f'FROM {base}\nCOPY claude /opt/claude\n'
        'COPY turnrun_native_bridge.py /opt/ringo/turnrun_native_bridge.py\n'
        f'LABEL ringo.native.version="{version}" ringo.native.sha256="{checksum}"\n')
    docker_platform = 'linux/arm64' if platform == 'linux-arm64' else 'linux/amd64'
    subprocess.run(['docker', 'build', '--network', 'none', '--platform', docker_platform,
                    '--iidfile', str(work / 'image-id.txt'), str(work)], check=True, timeout=300)
    image = (work / 'image-id.txt').read_text().strip()
    if not IMAGE.fullmatch(image):
        raise ValueError('Docker did not return an immutable image ID')
    result = {'image': image, 'base': base, 'version': version, 'platform': platform,
              'binary_sha256': checksum, 'bridge_sha256': sha256_file(bridge),
              'manifest_sha256': hashlib.sha256(manifest_path.read_bytes()).hexdigest(),
              'provider_calls': 0, 'live_certified': False}
    (work / 'build.json').write_text(json.dumps(result, indent=2) + '\n')
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--platform', required=True, choices=['linux-arm64', 'linux-x64'])
    parser.add_argument('--base', required=True)
    parser.add_argument('--work', required=True)
    print(json.dumps(build(**vars(parser.parse_args()))))
