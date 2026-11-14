"""Native subscription process boundary for future standalone RRSI execution.

No API-key route, owner home, task source, grader or general network is exposed to the
provider container. Offline probing is permitted in a child; inference always refuses it.
This is a process adapter, not certification of Session scenarios or a live campaign.
"""
import argparse
import base64
import ipaddress
import json
import os
from pathlib import Path
import queue
import socket
import stat
import subprocess
import threading
import time

from turnrun import ledger_append, outside_repo, sha256_file, usage_of
from turnrun_isolation import DockerSandbox

AUTHORITY = 'api.anthropic.com:443'
FRAME_BYTES = 512 * 1024
CHUNK_BYTES = 16384
USAGE_FIELDS = {'input': 'input_tokens', 'cache_write': 'cache_creation_input_tokens',
                'cache_read': 'cache_read_input_tokens', 'output': 'output_tokens'}


def retained_usage(events):
    """Preserve a complete aggregate, or a conservative observed per-message lower bound."""
    finals = [e for e in events if e.get('type') == 'result']
    if len(finals) == 1:
        aggregate = finals[0].get('usage')
        aggregate = aggregate if isinstance(aggregate, dict) else {}
        counts = {key: aggregate.get(field) for key, field in USAGE_FIELDS.items()}
        if all(type(n) is int and n >= 0 for n in counts.values()):
            return {**counts, 'counted': True, 'uncounted_calls': 0}
    messages = {}
    for event in events:
        message = event.get('message') if event.get('type') == 'assistant' else None
        if not isinstance(message, dict) or not isinstance(message.get('id'), str):
            continue
        row = messages.setdefault(message['id'], {key: 0 for key in USAGE_FIELDS})
        reported = message.get('usage')
        reported = reported if isinstance(reported, dict) else {}
        for key, field in USAGE_FIELDS.items():
            n = reported.get(field)
            if type(n) is int and n >= 0:
                row[key] = max(row[key], n)
    return {**{key: sum(row[key] for row in messages.values()) for key in USAGE_FIELDS},
            'counted': False, 'uncounted_calls': 1}


class NativeAttemptError(ValueError):
    def __init__(self, reason, events):
        super().__init__(reason)
        self.result = {'outcome': 'failed', 'usage': retained_usage(events)}


def oauth_token(path):
    if Path(path).is_symlink():
        raise ValueError('token must be a private ordinary file, not a symlink')
    path = outside_repo(path, 'dedicated subscription token')
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_size > 4096 or info.st_mode & 0o077:
        raise ValueError('token must be a private ordinary file (mode 0600)')
    token = path.read_text().strip()
    if not token.startswith('sk-ant-oat') or any(c.isspace() for c in token):
        raise ValueError('a dedicated native subscription OAuth token is required; API keys are refused')
    return token


def provider_socket(authority):
    if authority != AUTHORITY:
        raise ValueError('only the pinned provider authority is permitted')
    # Resolve in the trusted controller and reject private/link-local/loopback destinations.
    addresses = socket.getaddrinfo('api.anthropic.com', 443, type=socket.SOCK_STREAM)
    if not addresses or any(not ipaddress.ip_address(item[4][0]).is_global for item in addresses):
        raise ValueError('provider DNS returned a non-public destination')
    family, kind, protocol, _, address = addresses[0]
    sock = socket.socket(family, kind, protocol)
    try:
        sock.settimeout(10)
        sock.connect(address)
        sock.settimeout(1)
        return sock
    except BaseException:
        sock.close()
        raise


def native_result(events, code, model, worker_required=True):
    """Use aggregate final usage exactly once; missing/error output never becomes zero."""
    initial = [e for e in events if e.get('type') == 'system' and e.get('subtype') == 'init']
    finals = [e for e in events if e.get('type') == 'result']
    expected = ['mcp__trial__worker'] if worker_required else []
    if len(initial) != 1 or initial[0].get('tools') != expected:
        raise ValueError('native tool discovery differs from the fixed profile')
    servers = initial[0].get('mcp_servers', [])
    if worker_required and servers != [{'name': 'trial', 'status': 'connected'}]:
        raise ValueError('the fixed worker MCP server did not connect')
    if initial[0].get('model') != model or len(finals) != 1:
        raise ValueError('provider model or final result differs from the pinned request')
    final = finals[0]
    if set(final.get('modelUsage', {})) != {model}:
        raise ValueError('native usage includes an unpinned or unknown model')
    counts = final.get('usage')
    if not isinstance(counts, dict):
        raise ValueError('native aggregate usage is incomplete')
    usage = {key: counts.get(field) for key, field in USAGE_FIELDS.items()}
    if any(type(n) is not int or n < 0 for n in usage.values()):
        raise ValueError('native aggregate usage is incomplete')
    return {'outcome': 'done' if code == 0 and final.get('is_error') is False
            and final.get('subtype') == 'success' else 'failed',
            'usage': {**usage, 'counted': True, 'uncounted_calls': 0},
            'reply': final.get('result', ''), 'model': model,
            'model_turns': final.get('num_turns'), 'provider_requests': None}


class NativeBroker(DockerSandbox):
    """One disposable, network-disabled provider process, separate from worker and checker."""
    def probe(self):
        row = json.loads(self._call(['exec', self.id, 'python3', '-I',
                         '/opt/ringo/turnrun_native_bridge.py', '--probe'], timeout=30, limit=8192))
        required = ('direct_network_denied', 'owner_home_absent', 'docker_socket_absent',
                    'managed_settings_absent', 'task_workspace_empty')
        if row.get('uid') != 65532 or any(row.get(key) is not True for key in required):
            raise ValueError('native process containment probe failed')
        if row.get('bridge_sha256') != sha256_file(Path(__file__).parent / 'turnrun_native_bridge.py'):
            raise ValueError('native image bridge differs from the selected controller source')
        return {**row, 'image': self.image, 'provider_calls': 0, 'live_certified': False}

    def exercise_transport(self, endpoint, *, native=False):
        """Exercise real broker-container -> controller -> worker transport with no model."""
        self.probe()
        with subprocess.Popen([self.docker, 'exec', '-i', self.id, 'python3', '-I',
                               '/opt/ringo/turnrun_native_bridge.py', '--exercise-native' if native else '--exercise'],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as proc:
            messages = queue.Queue(maxsize=8)
            def read():
                try:
                    while line := proc.stdout.readline(FRAME_BYTES+1):
                        if len(line) > FRAME_BYTES or not line.endswith(b'\n'):
                            raise ValueError('oversized transport probe frame')
                        messages.put(json.loads(line), timeout=5)
                except Exception:
                    messages.put({'type': 'fault'}, timeout=5)
                finally:
                    messages.put({'type': 'eof'}, timeout=5)
            threading.Thread(target=read, daemon=True).start()
            deadline = time.monotonic()+45
            events, code = [], None
            try:
                while True:
                    frame = messages.get(timeout=max(0.01, deadline-time.monotonic()))
                    if frame.get('type') == 'mcp':
                        reply = {'type': 'mcp', 'id': frame['id'],
                                 'message': endpoint.dispatch(frame['message'])}
                        proc.stdin.write(json.dumps(reply).encode()+b'\n')
                        proc.stdin.flush()
                    elif native and frame.get('type') == 'event':
                        events.append(frame['event'])
                    elif native and frame.get('type') == 'exit':
                        code = frame['code']
                    elif native and frame.get('type') == 'offline-native':
                        result = native_result(events, code, 'claude-sonnet-4-6')
                        if result['outcome'] != 'done' or not frame['tool_result_seen'] or frame['replay_requests'] != 2:
                            raise ValueError('actual native CLI protocol replay failed')
                        proc.wait(timeout=5)
                        return {**frame, 'result': result}
                    elif frame.get('type') == 'exercise':
                        proc.wait(timeout=5)
                        if proc.returncode or frame['result'].get('error'):
                            raise ValueError(f'native transport probe failed (exit {proc.returncode})')
                        return frame
                    else:
                        raise ValueError(f'native transport probe failed ({frame.get("type")})')
            finally:
                if proc.poll() is None:
                    self._call(['stop', '--time', '1', self.id], timeout=15, limit=8192)
                    self.stopped = True
                    proc.kill()
                    proc.wait(timeout=10)

    def run(self, *, prompt, instruction, model, token_file, endpoint, token_cap,
            seconds=300, max_turns=8):
        if 'RINGO_CHILD' in os.environ:
            raise ValueError('live native execution refuses RINGO_CHILD; use a standalone session')
        if not isinstance(model, str) or not model.startswith('claude-') or len(model) > 128:
            raise ValueError('pin a full Claude model name')
        if not isinstance(prompt, str) or len(prompt.encode()) > 128 * 1024:
            raise ValueError('prompt exceeds its limit')
        if not isinstance(instruction, str) or len(instruction.encode()) > 4096:
            raise ValueError('instruction exceeds its limit')
        if type(seconds) is not int or not 1 <= seconds <= self.lifetime - 15:
            raise ValueError('native wall limit must fit the broker lease')
        if type(max_turns) is not int or not 1 <= max_turns <= 32:
            raise ValueError('native turn limit must be from 1 through 32')
        if type(token_cap) is not int or not 1 <= token_cap <= 10000000:
            raise ValueError('a numeric per-attempt token reservation is required')
        if endpoint.worker.id == self.id or endpoint.failed or not endpoint.server:
            raise ValueError('native broker requires a separate active worker endpoint')
        self.probe()
        request = {'prompt': prompt, 'instruction': instruction, 'model': model,
                   'oauth_token': oauth_token(token_file), 'max_turns': max_turns}
        command = [self.docker, 'exec', '-i', self.id, 'python3', '-I', '/opt/ringo/turnrun_native_bridge.py']
        frames = queue.Queue(maxsize=16)
        events, tunnels = [], {}
        writer_lock = threading.Lock()
        stop = threading.Event()
        with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL) as proc:
            def send(frame):
                data = json.dumps(frame, allow_nan=False).encode() + b'\n'
                if len(data) > FRAME_BYTES:
                    raise ValueError('controller frame too large')
                with writer_lock:
                    proc.stdin.write(data)
                    proc.stdin.flush()

            def read():
                try:
                    while line := proc.stdout.readline(FRAME_BYTES + 1):
                        if len(line) > FRAME_BYTES or not line.endswith(b'\n'):
                            raise ValueError('oversized native frame')
                        frames.put(json.loads(line), timeout=5)
                except Exception:
                    frames.put({'type': 'fault'}, timeout=5)
                finally:
                    frames.put({'type': 'eof'}, timeout=5)

            def upstream(key, conn):
                try:
                    while not stop.is_set():
                        try:
                            data = conn.recv(CHUNK_BYTES)
                        except socket.timeout:
                            continue
                        if not data:
                            break
                        send({'type': 'data', 'id': key, 'data': base64.b64encode(data).decode()})
                except (OSError, ValueError):
                    pass
                finally:
                    if not stop.is_set():
                        try:
                            send({'type': 'close', 'id': key})
                        except OSError:
                            pass

            total, network_bytes, opened, exit_code = 0, 0, set(), None
            deadline = time.monotonic() + seconds
            def expire():
                stop.set()
                try:
                    self._call(['stop', '--time', '1', self.id], timeout=15, limit=8192)
                except Exception:
                    pass  # The independent container lease is the final cleanup fallback.
            timer = threading.Timer(seconds, expire)
            timer.daemon = True
            try:
                timer.start()
                send(request)
                # The OAuth credential exists only in this pipe and the provider environment;
                # it never enters Docker argv, inspection metadata, task env or a trace.
                request.clear()
                threading.Thread(target=read, daemon=True).start()
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError('native wall limit reached; usage is unknown')
                    try:
                        frame = frames.get(timeout=remaining)
                    except queue.Empty:
                        raise TimeoutError('native wall limit reached; usage is unknown') from None
                    kind, key = frame.get('type'), frame.get('id')
                    if kind == 'event':
                        event = frame['event']
                        total += len(json.dumps(event).encode())
                        if total > 8 * 1024 * 1024 or not isinstance(event, dict):
                            raise ValueError('native event limit exceeded')
                        events.append(event)
                        if event.get('type') == 'system' and event.get('subtype') == 'init':
                            if event.get('tools') != ['mcp__trial__worker'] or event.get('model') != model:
                                raise ValueError('native startup exposed unexpected tools or model')
                    elif kind == 'mcp':
                        send({'type': 'mcp', 'id': key, 'message': endpoint.dispatch(frame['message'])})
                    elif kind == 'open':
                        if type(key) is not int or key in opened or len(opened) >= 256 or len(tunnels) >= 8:
                            raise ValueError('native tunnel admission limit reached')
                        conn = provider_socket(frame.get('authority'))
                        opened.add(key)
                        tunnels[key] = conn
                        send({'type': 'opened', 'id': key})
                        threading.Thread(target=upstream, args=(key, conn), daemon=True).start()
                    elif kind == 'data':
                        data = base64.b64decode(frame['data'], validate=True)
                        network_bytes += len(data)
                        if type(key) is not int or key not in opened or len(data) > CHUNK_BYTES or network_bytes > 32*1024*1024:
                            raise ValueError('invalid native tunnel chunk')
                        if key in tunnels:
                            tunnels[key].sendall(data)
                    elif kind == 'close':
                        conn = tunnels.pop(key, None)
                        if conn:
                            conn.close()
                    elif kind == 'exit':
                        exit_code = frame['code']
                        break
                    else:
                        raise ValueError('native bridge failed; preserve the full token reservation')
                proc.wait(timeout=max(0.1, deadline-time.monotonic()))
                result = native_result(events, exit_code, model)
                if sum(result['usage'][k] for k in ('input', 'cache_write', 'cache_read', 'output')) > token_cap:
                    result['outcome'] = 'budget'
                return result
            except Exception as exc:
                raise NativeAttemptError(type(exc).__name__+' during native attempt', events) from None
            finally:
                timer.cancel()
                stop.set()
                for conn in tunnels.values():
                    conn.close()
                # Kill the whole provider container, including descendants, on every outcome.
                cleanup_error = False
                try:
                    self._call(['stop', '--time', '1', self.id], timeout=15, limit=8192)
                    self.stopped = True
                except Exception:
                    cleanup_error = True
                finally:
                    if proc.poll() is None:
                        proc.kill()
                    proc.wait(timeout=10)
                if cleanup_error:
                    raise NativeAttemptError('native container cleanup failed', events)


def certify(*, image, worker_image, model, token_file, budget_tokens, seconds, work):
    """One reserved standalone proof trial; not a campaign or Session certification."""
    if 'RINGO_CHILD' in os.environ:
        raise ValueError('native certification must run outside a Ringo child session')
    if type(budget_tokens) is not int or not 1 <= budget_tokens <= 100000:
        raise ValueError('explicit certification admission budget must be from 1 through 100000 tokens')
    if type(seconds) is not int or not 30 <= seconds <= 300:
        raise ValueError('certification wall limit must be from 30 through 300 seconds')
    work = outside_repo(work, 'native certification evidence')
    # Fail before starting/reserving if authentication has not been provisioned.
    oauth_token(token_file)
    work.mkdir(parents=True, exist_ok=False, mode=0o700)
    sources = {name: sha256_file(Path(__file__).parent / name) for name in
               ('turnrun_native.py', 'turnrun_native_bridge.py', 'turnrun_broker.py', 'turnrun_isolation.py')}
    plan = {'image': image, 'worker_image': worker_image, 'model': model,
            'budget_tokens': budget_tokens, 'seconds': seconds, 'max_turns': 4, 'sources': sources,
            'authorization_source': 'explicit standalone CLI --budget-tokens',
            'scope': 'one native authentication/tool-discovery/worker proof; no campaign or Session certification'}
    (work / 'plan.json').write_text(json.dumps(plan, indent=2)+'\n')
    ledger = work / 'ledger.jsonl'
    ledger_append(ledger, {'kind': 'start', 'cap': budget_tokens, 'plan_sha256': sha256_file(work / 'plan.json')})
    # A fresh nonce proves this trial's worker actually executed the fixed tool.
    import secrets
    from turnrun_broker import WorkerEndpoint
    nonce = secrets.token_hex(24)
    report = {'process_profile_certified': False, 'live_campaign_certified': False,
              'provider_requests': None, 'known_tokens': None, 'reserved_tokens': budget_tokens,
              'image': image, 'worker_image': worker_image, 'model': model, 'sources': sources}
    try:
        with DockerSandbox(worker_image, lifetime=seconds+60) as worker:
            with WorkerEndpoint(worker, lifetime=seconds+30) as endpoint:
                with NativeBroker(image, lifetime=seconds+45) as broker:
                    result = broker.run(prompt='Use the worker tool to write exactly '+nonce+
                        ' followed by a newline to /work/native-proof.txt. Then finish.',
                        instruction='Use only the provided worker tool to complete the task.',
                        model=model, token_file=token_file, endpoint=endpoint,
                        token_cap=budget_tokens, seconds=seconds, max_turns=4)
                usage, total = usage_of(result)
                report.update(known_tokens=total, reserved_tokens=0 if total is not None else budget_tokens,
                              result=result, worker_calls=endpoint.calls)
                ledger_append(ledger, {'kind': 'finish', 'tokens': total, 'result': result})
                if result['outcome'] != 'done' or total is None or total > budget_tokens:
                    ledger_append(ledger, {'kind': 'halt', 'reason': 'usage or delivery gate failed'})
            submitted = worker.submission(work / 'submission')
            proof = submitted / 'native-proof.txt'
            valid = proof.is_file() and proof.read_bytes() == (nonce+'\n').encode()
            pins_valid = all(sha256_file(Path(__file__).parent / name) == value for name, value in sources.items())
            report['process_profile_certified'] = bool(valid and pins_valid and endpoint.calls > 0
                and not endpoint.failed and result['outcome'] == 'done' and total is not None and total <= budget_tokens)
            ledger_append(ledger, {'kind': 'complete', 'process_profile_certified': report['process_profile_certified'],
                                   'live_campaign_certified': False})
    except Exception as exc:
        if isinstance(exc, NativeAttemptError):
            counts, total = usage_of(exc.result)
            lower_bound = sum(counts.values())
            report.update(result=exc.result, known_tokens=total,
                          reserved_tokens=0 if total is not None else max(budget_tokens, lower_bound))
            ledger_append(ledger, {'kind': 'finish', 'tokens': total, 'lower_bound': lower_bound,
                                   'result': exc.result})
        report['error'] = type(exc).__name__
        ledger_append(ledger, {'kind': 'halt', 'reason': type(exc).__name__,
                               'reserved_tokens': report['reserved_tokens']})
    (work / 'report.json').write_text(json.dumps(report, indent=2)+'\n')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    probe = sub.add_parser('probe', help='zero-inference native containment probe')
    probe.add_argument('--image', required=True)
    probe.add_argument('--report', required=True)
    live = sub.add_parser('certify', help='one explicitly budgeted native proof in a standalone session')
    for key in ('image', 'worker-image', 'model', 'token-file', 'work'):
        live.add_argument('--'+key, required=True)
    live.add_argument('--budget-tokens', type=int, required=True)
    live.add_argument('--seconds', type=int, default=120)
    args = parser.parse_args()
    if args.command == 'certify':
        kwargs = vars(args)
        kwargs.pop('command')
        result = certify(**kwargs)
        print(json.dumps({k: v for k, v in result.items() if k != 'result'}))
        return 0 if result['process_profile_certified'] else 2
    report = outside_repo(args.report, 'native probe report')
    with NativeBroker(args.image, lifetime=90) as broker:
        result = broker.probe()
    result['sources'] = {name: sha256_file(Path(__file__).parent / name) for name in
                         ('turnrun_native.py', 'turnrun_native_bridge.py', 'turnrun_isolation.py')}
    report.parent.mkdir(parents=True, exist_ok=True)
    with report.open('x') as out:
        json.dump(result, out, indent=2)
        out.write('\n')
    print(json.dumps(result))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
