/** 演示数据。确定性伪随机，每次打开图形一致，便于逐版比对。 */
export function mulberry32(seed: number): () => number {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function series(seed: number, n: number, base: number, amp: number): number[] {
  const rand = mulberry32(seed);
  let v = base;
  const out: number[] = [];
  for (let i = 0; i < n; i++) {
    v += (rand() - 0.5) * amp + Math.sin(i / 7) * amp * 0.22;
    v = Math.min(99, Math.max(1, v));
    out.push(v);
  }
  return out;
}

export interface Service {
  name: string;
  state: "run" | "stop" | "fail";
  enabled: string;
  desc: string;
  cpu: string;
  mem: string;
  source: string;
}

export const SERVICES: readonly Service[] = [
  {
    name: "nginx.service",
    state: "run",
    enabled: "开机自启",
    desc: "Web 服务器与反向代理",
    cpu: "2.4",
    mem: "148 MiB",
    source: "systemd",
  },
  {
    name: "containerd.service",
    state: "fail",
    enabled: "开机自启",
    desc: "容器运行时",
    cpu: "0.0",
    mem: "—",
    source: "systemd",
  },
  {
    name: "sshd.service",
    state: "run",
    enabled: "开机自启",
    desc: "OpenSSH 远程登录",
    cpu: "0.1",
    mem: "12 MiB",
    source: "systemd",
  },
  {
    name: "postgresql.service",
    state: "run",
    enabled: "开机自启",
    desc: "PostgreSQL 数据库集群",
    cpu: "7.8",
    mem: "1.9 GiB",
    source: "systemd",
  },
  {
    name: "cron.service",
    state: "run",
    enabled: "开机自启",
    desc: "定时任务调度",
    cpu: "0.0",
    mem: "3 MiB",
    source: "systemd",
  },
  {
    name: "docker.service",
    state: "fail",
    enabled: "已禁用",
    desc: "Docker 容器引擎",
    cpu: "0.0",
    mem: "—",
    source: "systemd",
  },
  {
    name: "systemd-resolved.service",
    state: "run",
    enabled: "开机自启",
    desc: "网络名称解析",
    cpu: "0.2",
    mem: "9 MiB",
    source: "systemd",
  },
  {
    name: "chronyd.service",
    state: "run",
    enabled: "开机自启",
    desc: "网络时间同步",
    cpu: "0.0",
    mem: "4 MiB",
    source: "systemd",
  },
  {
    name: "rsyslog.service",
    state: "stop",
    enabled: "已禁用",
    desc: "系统日志转发",
    cpu: "0.0",
    mem: "—",
    source: "systemd",
  },
  {
    name: "ufw.service",
    state: "run",
    enabled: "开机自启",
    desc: "防火墙前端",
    cpu: "0.0",
    mem: "2 MiB",
    source: "systemd",
  },
  {
    name: "snapd.service",
    state: "run",
    enabled: "开机自启",
    desc: "Snap 软件包守护进程",
    cpu: "0.3",
    mem: "64 MiB",
    source: "systemd",
  },
  {
    name: "unattended-upgrades.service",
    state: "stop",
    enabled: "开机自启",
    desc: "无人值守安全更新",
    cpu: "0.0",
    mem: "—",
    source: "systemd",
  },
];
