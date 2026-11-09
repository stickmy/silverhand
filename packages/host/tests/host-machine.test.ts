import { describe, expect, it } from 'vitest';
import {
  parseDarwinMemory,
  parseDarwinRouteInterface,
  parseLinuxMemory,
  parseLinuxRouteInterface,
} from '../src/daemon/host-machine.js';

describe('host machine facts parsing', () => {
  it('reads the default-route interface on macOS', () => {
    const output =
      '   route to: default\ndestination: default\n  gateway: 10.0.0.1\n  interface: en0\n';
    expect(parseDarwinRouteInterface(output)).toBe('en0');
    expect(parseDarwinRouteInterface('route: writing to routing socket: not in table')).toBe(
      undefined,
    );
  });

  it('picks the lowest-metric default route on Linux', () => {
    const table = [
      'Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT',
      'wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0',
      'eth0\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0',
      'eth0\t0000000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0',
    ].join('\n');
    expect(parseLinuxRouteInterface(table)).toBe('eth0');
    expect(parseLinuxRouteInterface(table.split('\n')[0] ?? '')).toBe(undefined);
  });

  it('counts app + wired + compressed memory on macOS, not file cache', () => {
    const vmStat = [
      'Mach Virtual Memory Statistics: (page size of 16384 bytes)',
      'Pages free:                                   100.',
      'Pages wired down:                             200.',
      'Pages purgeable:                               50.',
      'File-backed pages:                           9000.',
      'Anonymous pages:                             1050.',
      'Pages occupied by compressor:                 300.',
    ].join('\n');
    expect(parseDarwinMemory(vmStat, 1_000_000_000)).toEqual({
      total: 1_000_000_000,
      used: (1000 + 200 + 300) * 16384,
    });
    expect(parseDarwinMemory('garbage', 1_000_000_000)).toBe(undefined);
  });

  it('uses MemTotal − MemAvailable on Linux', () => {
    const meminfo = 'MemTotal:       16000 kB\nMemFree:  1000 kB\nMemAvailable:    6000 kB\n';
    expect(parseLinuxMemory(meminfo)).toEqual({ total: 16000 * 1024, used: 10000 * 1024 });
    expect(parseLinuxMemory('MemTotal: 1 kB\n')).toBe(undefined);
  });
});
