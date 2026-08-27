//! 进程列表的筛选、排序与树视图（[`ProcessListQuery`] 的语义）。
//!
//! 全部是对 `Vec<ProcessSummary>` 的纯操作，不碰 `/proc`，便于测试。

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use strixmaid_types::process::{ProcessListQuery, ProcessSortKey, ProcessSummary, SortOrder};

/// 按查询参数处理完整的进程列表。
///
/// `resolve_uid` 把用户名解析成 uid（查不到返回 `None`）。
///
/// `tree = true` 时：凡命中 `q` / `user` 的进程，其全部祖先一并保留（否则前端拼树会断链），
/// 并按父子关系深度优先排序，同级兄弟按 `sort` / `order` 排。响应形状不变（平铺数组）。
pub fn apply(
    all: Vec<ProcessSummary>,
    query: &ProcessListQuery,
    resolve_uid: impl Fn(&str) -> Option<u32>,
) -> Vec<ProcessSummary> {
    let uid_filter = query.user.as_deref().map(|u| match u.parse::<u32>() {
        Ok(uid) => Some(uid),
        Err(_) => resolve_uid(u),
    });
    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);

    let matches = |p: &ProcessSummary| -> bool {
        if let Some(uid_filter) = &uid_filter {
            match uid_filter {
                Some(uid) => {
                    if p.uid != *uid {
                        return false;
                    }
                }
                // 用户名解析不到：按用户名字段兜底匹配（几乎必然为空结果，但语义正确）
                None => {
                    if p.user.as_deref() != query.user.as_deref() {
                        return false;
                    }
                }
            }
        }
        if let Some(needle) = &needle {
            let in_name = p.name.to_lowercase().contains(needle.as_str());
            let in_cmd = p
                .cmdline
                .as_deref()
                .is_some_and(|c| c.to_lowercase().contains(needle.as_str()));
            if !in_name && !in_cmd {
                return false;
            }
        }
        true
    };

    let sort_key = query.sort.unwrap_or_default();
    let order = query.order.unwrap_or_else(|| default_order(sort_key));
    let cmp = |a: &ProcessSummary, b: &ProcessSummary| compare(sort_key, order, a, b);

    if query.tree.unwrap_or(false) {
        let has_filter = uid_filter.is_some() || needle.is_some();
        let keep: HashSet<u32> = if has_filter {
            with_ancestors(&all, |p| matches(p))
        } else {
            all.iter().map(|p| p.pid).collect()
        };
        let kept: Vec<ProcessSummary> = all.into_iter().filter(|p| keep.contains(&p.pid)).collect();
        tree_order(kept, cmp)
    } else {
        let mut kept: Vec<ProcessSummary> = all.into_iter().filter(matches).collect();
        kept.sort_by(cmp);
        kept
    }
}

/// 未指定方向时的默认：数值型（CPU / 内存 / 启动时刻）降序——先看最占资源、最新的；
/// 文本型与 pid 升序。
pub fn default_order(key: ProcessSortKey) -> SortOrder {
    match key {
        ProcessSortKey::Cpu | ProcessSortKey::Mem | ProcessSortKey::StartTs => SortOrder::Desc,
        ProcessSortKey::Pid | ProcessSortKey::Name | ProcessSortKey::User => SortOrder::Asc,
    }
}

/// 按排序键比较；同值时按 pid 升序保证输出稳定。
pub fn compare(
    key: ProcessSortKey,
    order: SortOrder,
    a: &ProcessSummary,
    b: &ProcessSummary,
) -> Ordering {
    let primary = match key {
        ProcessSortKey::Pid => a.pid.cmp(&b.pid),
        ProcessSortKey::Name => cmp_ignore_case(&a.name, &b.name),
        ProcessSortKey::Cpu => a
            .cpu_percent
            .partial_cmp(&b.cpu_percent)
            .unwrap_or(Ordering::Equal),
        ProcessSortKey::Mem => a.rss_bytes.cmp(&b.rss_bytes),
        ProcessSortKey::User => {
            let ua = a.user.clone().unwrap_or_else(|| a.uid.to_string());
            let ub = b.user.clone().unwrap_or_else(|| b.uid.to_string());
            cmp_ignore_case(&ua, &ub)
        }
        ProcessSortKey::StartTs => a.start_ts.cmp(&b.start_ts),
    };
    let primary = match order {
        SortOrder::Asc => primary,
        SortOrder::Desc => primary.reverse(),
    };
    primary.then_with(|| a.pid.cmp(&b.pid))
}

fn cmp_ignore_case(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
        .then_with(|| a.cmp(b))
}

/// 命中项 + 它们的全部祖先。
fn with_ancestors(all: &[ProcessSummary], matches: impl Fn(&ProcessSummary) -> bool) -> HashSet<u32> {
    let parent: HashMap<u32, u32> = all.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut keep = HashSet::new();
    for p in all.iter().filter(|p| matches(p)) {
        let mut cur = p.pid;
        // 沿 ppid 向上，直到根或已访问；ppid 不在列表里（已退出）时到此为止
        while keep.insert(cur) {
            match parent.get(&cur) {
                Some(&pp) if pp != 0 && pp != cur && parent.contains_key(&pp) => cur = pp,
                _ => break,
            }
        }
    }
    keep
}

/// 深度优先排列：父在前、子紧随其后，兄弟间按 `cmp`。
///
/// 根 = ppid 为 0 或父不在集合里的进程（被过滤掉祖先时也能成为根，树不会丢节点）。
fn tree_order(
    procs: Vec<ProcessSummary>,
    cmp: impl Fn(&ProcessSummary, &ProcessSummary) -> Ordering,
) -> Vec<ProcessSummary> {
    let present: HashSet<u32> = procs.iter().map(|p| p.pid).collect();
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, p) in procs.iter().enumerate() {
        if p.ppid != 0 && p.ppid != p.pid && present.contains(&p.ppid) {
            children.entry(p.ppid).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    let sort_idx = |v: &mut Vec<usize>| v.sort_by(|&a, &b| cmp(&procs[a], &procs[b]));
    sort_idx(&mut roots);
    for v in children.values_mut() {
        sort_idx(v);
    }

    let mut order: Vec<usize> = Vec::with_capacity(procs.len());
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    let mut visited: HashSet<usize> = HashSet::with_capacity(procs.len());
    while let Some(i) = stack.pop() {
        if !visited.insert(i) {
            continue;
        }
        order.push(i);
        if let Some(kids) = children.get(&procs[i].pid) {
            for &k in kids.iter().rev() {
                stack.push(k);
            }
        }
    }

    // 把 Vec 按 order 重排：用 Option 占位避免克隆
    let mut slots: Vec<Option<ProcessSummary>> = procs.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::process::ProcessState;

    fn p(pid: u32, ppid: u32, name: &str, uid: u32, cpu: f64, rss: u64) -> ProcessSummary {
        ProcessSummary {
            pid,
            ppid,
            name: name.into(),
            cmdline: Some(format!("/usr/bin/{name} --flag")),
            uid,
            user: Some(if uid == 0 { "root".into() } else { format!("u{uid}") }),
            state: ProcessState::Sleeping,
            cpu_percent: cpu,
            rss_bytes: rss,
            vms_bytes: rss * 2,
            mem_percent: 0.0,
            threads: 1,
            start_ts: 1000 + pid as i64,
            nice: 0,
        }
    }

    /// 1(systemd) ── 2(sshd) ── 3(bash) ── 4(nginx)
    ///            └─ 5(cron)            └─ 6(vim)
    fn fixture() -> Vec<ProcessSummary> {
        vec![
            p(1, 0, "systemd", 0, 0.1, 10),
            p(2, 1, "sshd", 0, 0.2, 20),
            p(3, 2, "bash", 1000, 0.0, 30),
            p(4, 3, "nginx", 33, 5.0, 400),
            p(5, 1, "cron", 0, 0.0, 5),
            p(6, 3, "vim", 1000, 1.0, 60),
        ]
    }

    fn pids(v: &[ProcessSummary]) -> Vec<u32> {
        v.iter().map(|p| p.pid).collect()
    }

    #[test]
    fn 默认按_cpu_降序() {
        let out = apply(fixture(), &ProcessListQuery::default(), |_| None);
        assert_eq!(pids(&out), vec![4, 6, 2, 1, 3, 5]);
    }

    #[test]
    fn 名字升序_pid_兜底() {
        let q = ProcessListQuery {
            sort: Some(ProcessSortKey::Name),
            ..Default::default()
        };
        let out = apply(fixture(), &q, |_| None);
        assert_eq!(pids(&out), vec![3, 5, 4, 2, 1, 6]);
        let q = ProcessListQuery {
            sort: Some(ProcessSortKey::Mem),
            order: Some(SortOrder::Asc),
            ..Default::default()
        };
        let out = apply(fixture(), &q, |_| None);
        assert_eq!(pids(&out), vec![5, 1, 2, 3, 6, 4]);
    }

    #[test]
    fn 用户过滤_数字与名字() {
        let q = ProcessListQuery {
            user: Some("1000".into()),
            ..Default::default()
        };
        let out = apply(fixture(), &q, |_| None);
        assert_eq!(pids(&out), vec![6, 3]);
        let q = ProcessListQuery {
            user: Some("www-data".into()),
            ..Default::default()
        };
        let out = apply(fixture(), &q, |n| (n == "www-data").then_some(33));
        assert_eq!(pids(&out), vec![4]);
        // 解析不到的用户名 → 空
        let out = apply(fixture(), &q, |_| None);
        assert!(out.is_empty());
    }

    #[test]
    fn 关键字大小写不敏感且匹配_cmdline() {
        let q = ProcessListQuery {
            q: Some("NGINX".into()),
            ..Default::default()
        };
        assert_eq!(pids(&apply(fixture(), &q, |_| None)), vec![4]);
        let q = ProcessListQuery {
            q: Some("--flag".into()),
            sort: Some(ProcessSortKey::Pid),
            ..Default::default()
        };
        assert_eq!(apply(fixture(), &q, |_| None).len(), 6);
    }

    #[test]
    fn 树视图保留祖先并深度优先() {
        let q = ProcessListQuery {
            q: Some("vim".into()),
            tree: Some(true),
            ..Default::default()
        };
        let out = apply(fixture(), &q, |_| None);
        // vim(6) 的祖先 3 → 2 → 1 都在，且父在子前
        assert_eq!(pids(&out), vec![1, 2, 3, 6]);

        // 无过滤的树：兄弟按 cpu 降序（sshd 0.2 > cron 0.0；nginx 5.0 > vim 1.0）
        let q = ProcessListQuery {
            tree: Some(true),
            ..Default::default()
        };
        let out = apply(fixture(), &q, |_| None);
        assert_eq!(pids(&out), vec![1, 2, 3, 4, 6, 5]);
    }

    #[test]
    fn 树视图父不存在时成为根() {
        let mut v = fixture();
        v.retain(|p| p.pid != 2); // sshd 消失
        let q = ProcessListQuery {
            tree: Some(true),
            sort: Some(ProcessSortKey::Pid),
            ..Default::default()
        };
        let out = apply(v, &q, |_| None);
        assert_eq!(out.len(), 5);
        assert_eq!(pids(&out), vec![1, 5, 3, 4, 6]);
    }
}
