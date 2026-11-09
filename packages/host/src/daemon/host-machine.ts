/**
 * Machine-wide facts for the `host_status` snapshot (#task:2991). Read live on
 * every call; a fact that cannot be read is omitted, never zeroed. Never throws.
 */
import { execFileSync } from 'node:child_process';
import { readFileSync, statfsSync } from 'node:fs';
import { hostname, networkInterfaces, platform, totalmem, uptime } from 'node:os';

export interface HostMachineFacts {
  hostname?: string;
  ip?: string;
  memory_total_bytes?: number;
  memory_used_bytes?: number;
  disk_total_bytes?: number;
  disk_used_bytes?: number;
  system_uptime_ms?: number;
}

export function readHostMachineFacts(): HostMachineFacts {
  const facts: HostMachineFacts = {};
  const name = attempt(() => hostname().trim());
  if (name) facts.hostname = name;
  const ip = attempt(defaultRouteIpv4);
  if (ip) facts.ip = ip;
  const memory = attempt(readMemory);
  if (memory) {
    facts.memory_total_bytes = memory.total;
    facts.memory_used_bytes = memory.used;
  }
  const disk = attempt(readSystemDisk);
  if (disk) {
    facts.disk_total_bytes = disk.total;
    facts.disk_used_bytes = disk.used;
  }
  const up = attempt(() => Math.round(uptime() * 1000));
  if (isCount(up)) facts.system_uptime_ms = up;
  return facts;
}

function attempt<T>(read: () => T): T | undefined {
  try {
    return read();
  } catch {
    return undefined;
  }
}

function isCount(value: number | undefined): value is number {
  return value !== undefined && Number.isSafeInteger(value) && value >= 0;
}

function run(command: string, args: string[]): string {
  return execFileSync(command, args, {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'ignore'],
    timeout: 1000,
  });
}

/** IPv4 of the interface carrying the default route; not the egress/public IP. */
export function defaultRouteIpv4(): string | undefined {
  const iface = defaultRouteInterface();
  if (!iface) return undefined;
  const address = networkInterfaces()[iface]?.find(
    (entry) => entry.family === 'IPv4' && !entry.internal,
  );
  return address?.address;
}

function defaultRouteInterface(): string | undefined {
  if (platform() === 'darwin') {
    return parseDarwinRouteInterface(run('route', ['-n', 'get', 'default']));
  }
  if (platform() === 'linux') {
    return parseLinuxRouteInterface(readFileSync('/proc/net/route', 'utf8'));
  }
  return undefined;
}

export function parseDarwinRouteInterface(output: string): string | undefined {
  return output.match(/^\s*interface:\s*(\S+)\s*$/m)?.[1];
}

/** Lowest-metric `00000000` destination row of `/proc/net/route`. */
export function parseLinuxRouteInterface(table: string): string | undefined {
  let best: { iface: string; metric: number } | undefined;
  for (const line of table.split('\n').slice(1)) {
    const [iface, destination, , , , , metric, mask] = line.trim().split(/\s+/);
    if (!iface || destination !== '00000000' || mask !== '00000000') continue;
    const value = Number.parseInt(metric ?? '', 10);
    const rank = Number.isFinite(value) ? value : Number.MAX_SAFE_INTEGER;
    if (!best || rank < best.metric) best = { iface, metric: rank };
  }
  return best?.iface;
}

interface Usage {
  total: number;
  used: number;
}

function readMemory(): Usage | undefined {
  if (platform() === 'darwin') {
    return parseDarwinMemory(run('vm_stat', []), totalmem());
  }
  if (platform() === 'linux') {
    return parseLinuxMemory(readFileSync('/proc/meminfo', 'utf8'));
  }
  return undefined;
}

/**
 * Activity Monitor's "Memory Used": app memory (anonymous − purgeable) + wired +
 * compressor-occupied. `os.freemem()` would count file cache as used.
 */
export function parseDarwinMemory(vmStat: string, total: number): Usage | undefined {
  const pageSize = Number(vmStat.match(/page size of (\d+) bytes/)?.[1]);
  const pages = (label: string): number | undefined => {
    const escaped = label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const raw = vmStat.match(new RegExp(`^${escaped}:\\s+(\\d+)\\.?\\s*$`, 'm'))?.[1];
    return raw === undefined ? undefined : Number(raw);
  };
  const anonymous = pages('Anonymous pages');
  const purgeable = pages('Pages purgeable');
  const wired = pages('Pages wired down');
  const compressed = pages('Pages occupied by compressor');
  if (
    !isCount(pageSize) ||
    !isCount(total) ||
    !isCount(anonymous) ||
    !isCount(purgeable) ||
    !isCount(wired) ||
    !isCount(compressed)
  ) {
    return undefined;
  }
  const used = (Math.max(0, anonymous - purgeable) + wired + compressed) * pageSize;
  return { total, used: Math.min(used, total) };
}

/** `MemTotal − MemAvailable`, matching `free`'s "used" on modern kernels. */
export function parseLinuxMemory(meminfo: string): Usage | undefined {
  const kib = (key: string): number | undefined => {
    const raw = meminfo.match(new RegExp(`^${key}:\\s+(\\d+)\\s+kB`, 'm'))?.[1];
    return raw === undefined ? undefined : Number(raw) * 1024;
  };
  const total = kib('MemTotal');
  const available = kib('MemAvailable');
  if (!isCount(total) || !isCount(available)) return undefined;
  return { total, used: Math.max(0, total - available) };
}

/** System volume `/`; on APFS this reports the shared container's space. */
function readSystemDisk(): Usage | undefined {
  const stats = statfsSync('/');
  const total = stats.blocks * stats.bsize;
  const available = stats.bavail * stats.bsize;
  if (!isCount(total) || !isCount(available)) return undefined;
  return { total, used: Math.max(0, total - available) };
}
