-- roadmap/08 §4.3：采集项从 58 裁到 34 之后的一次性清理。
-- 老库里被裁掉的 series 若不删除，会作为永不更新的孤儿常驻 series 表与五层桶。
-- 名单逐字列出（§4.3 的要求：不按前缀匹配，避免误伤未来的同前缀新指标）。
-- 连接始终开启 PRAGMA foreign_keys（store/mod.rs 的连接选项），五张 m_* 表对
-- series(id) 声明了 ON DELETE CASCADE：删 series 行即连带清空其全部桶数据。
DELETE FROM series WHERE metric IN (
  'cpu.user', 'cpu.nice', 'cpu.idle', 'cpu.softirq',
  'cpu.core.user', 'cpu.core.nice', 'cpu.core.system', 'cpu.core.idle',
  'cpu.core.iowait', 'cpu.core.irq', 'cpu.core.softirq', 'cpu.core.steal',
  'mem.free', 'mem.buffers', 'mem.dirty', 'mem.swap_free',
  'load.5m', 'load.15m',
  'psi.cpu.full',
  'disk.read_iops', 'disk.write_iops',
  'fs.usage', 'fs.inodes_used', 'fs.inodes_total',
  'net.rx_packets', 'net.tx_packets', 'net.rx_errors', 'net.tx_errors',
  'net.rx_drops', 'net.tx_drops'
);
