# SPDX-License-Identifier: GPL-3.0-or-later
"""Sequential release measurements: disk/HTTP x native/resampled x PCM capacity.

Requires an audible output device. Each case uses a fresh private audio cache.
A 44.1 kHz prelude requests the output rate; inputs are 44.1 and 48 kHz.
The measured output rate must confirm the requested native/resampled case.
"""
import argparse
import csv
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import tempfile
import threading
import time

import measure

STATS = re.compile(r'Статистика: consumed=(\d+) underruns=(\d+) callbacks=(\d+) requests=(\d+) bytes=(\d+) rate=(\d+)')


class RangeHandler(http.server.BaseHTTPRequestHandler):
    audio = None

    def log_message(self, *args):
        pass

    def do_GET(self):
        size = self.audio.stat().st_size
        match = re.fullmatch(r'bytes=(\d+)-(\d+)', self.headers.get('Range', ''))
        start, end = (int(match[1]), min(int(match[2]) + 1, size)) if match else (0, size)
        if start >= size:
            self.send_response(416)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        self.send_response(206 if match else 200)
        self.send_header('Content-Length', str(end - start))
        self.send_header('Content-Range', f'bytes {start}-{end-1}/{size}')
        self.send_header('ETag', '"synthetic-v1"')
        self.end_headers()
        try:
            with self.audio.open('rb') as stream:
                stream.seek(start)
                while start < end:
                    block = stream.read(min(65536, end - start))
                    self.wfile.write(block)
                    start += len(block)
        except (BrokenPipeError, ConnectionResetError):
            pass


def stats_delta(start, end):
    delta = [b-a for a,b in zip(start,end)]
    if any(x < 0 for x in delta):
        raise ValueError('Audio counters decreased')
    return delta


def measure_case(args, source, input_rate, capacity, sampler, metric, writer, stream):
    with tempfile.TemporaryDirectory(prefix='eagle-audio-matrix-') as directory:
        child = subprocess.Popen([str(args.binary.resolve()), 'play', str(args.prelude), source,
                                  '--buffer-seconds', str(capacity), '--stats'],
                                 env=dict(os.environ, EAGLE_DATA_DIR=directory, EAGLE_LOG='off'),
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        state = {'stats': None}
        condition = threading.Condition()

        def observe():
            for line in child.stdout:
                match = STATS.search(line)
                if match:
                    with condition:
                        state['stats'] = tuple(map(int, match.groups()))
                        condition.notify_all()

        observer = threading.Thread(target=observe, daemon=True)
        observer.start()
        try:
            with condition:
                if not condition.wait_for(lambda: state['stats'] is not None, timeout=15):
                    raise RuntimeError('No audio statistics within 15 seconds')
            time.sleep(args.warmup)
            for window in range(args.windows):
                before = sampler(child.pid)
                stats_before = state['stats']
                started = time.monotonic()
                peak = before[1]
                while time.monotonic() - started < args.seconds:
                    time.sleep(max(0, min(1, args.seconds - (time.monotonic() - started))))
                    if child.poll() is not None:
                        raise RuntimeError('Audio exited before the window ended')
                    peak = max(peak, sampler(child.pid)[1])
                after = sampler(child.pid)
                elapsed = time.monotonic() - started
                current = state['stats']
                if current[5] != 44100:
                    raise RuntimeError(f'Expected 44100 Hz output, got {current[5]}')
                delta = stats_delta(stats_before[:5], current[:5])
                if delta[0] <= 0:
                    raise RuntimeError('Music did not advance')
                row = [source.startswith('http'), input_rate, capacity, window+1, elapsed,
                       (after[0]-before[0])*100/elapsed, after[1], peak,
                       *delta, *measure.wakeup_deltas(before, after), *current[3:5], current[5]]
                writer.writerow(row)
                stream.flush()
                print(json.dumps(dict(zip(HEADERS,row))), flush=True)
        finally:
            child.terminate()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
            observer.join(timeout=2)
            errors = child.stderr.read()
            if errors:
                print(errors, flush=True)


HEADERS = ['http','input_rate','buffer_seconds','window','duration_seconds','cpu_percent_one_core',
           'memory_end_bytes','sampled_peak_memory_bytes','music_frames','underruns','callbacks',
           'http_requests','response_body_bytes','package_idle_wakeups','interrupt_wakeups',
           'run_http_requests','run_response_body_bytes','output_rate']


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, default=Path('target/release/eagle-cli'))
    parser.add_argument('--audio', type=Path, required=True, help='44.1 kHz MP3 longer than all windows')
    parser.add_argument('--audio-resampled', type=Path, required=True, help='48 kHz MP3 longer than all windows')
    parser.add_argument('--prelude', type=Path, required=True, help='44.1 kHz prelude')
    parser.add_argument('--seconds', type=float, default=60)
    parser.add_argument('--windows', type=int, default=3)
    parser.add_argument('--warmup', type=float, default=3)
    parser.add_argument('--buffers', default='2,5,10,20')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    capacities = [int(x) for x in args.buffers.split(',')]
    if args.seconds <= 0 or args.windows <= 0 or args.warmup < 0 or any(not 2 <= x <= 20 for x in capacities):
        parser.error('Invalid measurement parameters')
    system = platform.system()
    sampler, metric = {'Darwin':(measure.mac_sample,'phys_footprint_bytes'),
                       'Linux':(measure.linux_sample,'pss_bytes'),
                       'Windows':(measure.windows_sample,'private_working_set_bytes')}[system]
    RangeHandler.audio = args.audio.resolve()
    server = http.server.ThreadingHTTPServer(('127.0.0.1',0),RangeHandler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    metadata = {'os':platform.platform(),'machine':platform.machine(),'memory_metric':metric,
                'binary_sha256':hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                'audio_sha256':hashlib.sha256(args.audio.read_bytes()).hexdigest(),
                'audio_resampled_sha256':hashlib.sha256(args.audio_resampled.read_bytes()).hexdigest(),
                'git_revision':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),
                'git_dirty':bool(subprocess.check_output(['git','status','--porcelain'],text=True)),
                'rustc':subprocess.check_output(['rustc','-Vv'],text=True),
                'cpu_units':'percent of one core','radio_wakeups_measured':False,
                'counter_timing':'latest engine statistics, up to one second behind OS snapshots',
                'arguments':{k:str(v) if isinstance(v,Path) else v for k,v in vars(args).items()}}
    args.output.with_suffix('.json').write_text(json.dumps(metadata,indent=2)+'\n')
    try:
        with args.output.open('w',newline='') as stream:
            writer = csv.writer(stream)
            writer.writerow(HEADERS)
            for capacity in capacities:
                for audio, input_rate in [(args.audio,44100),(args.audio_resampled,48000)]:
                    RangeHandler.audio = audio.resolve()
                    for source in [str(audio.resolve()),f'http://127.0.0.1:{server.server_port}/tone.mp3']:
                        measure_case(args,source,input_rate,capacity,sampler,metric,writer,stream)
    finally:
        server.shutdown()
        server.server_close()
        worker.join()


if __name__ == '__main__':
    main()
