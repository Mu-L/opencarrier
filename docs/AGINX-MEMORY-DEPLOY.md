# aginxMemory 部署上线 Runbook

把 opencarrier 记忆子系统（kv+tree）从进程内 SQLite 外置到 aginxMemory（PG + md）。
**一次性切换，有停机 + 迁移线上数据**。所有步骤在服务器 `86quan`（`ubuntu@carrier.yinnho.cn`）执行。

## 前置确认

- aginx-memory 代码已 `git push deploy main`（hook 现在还只 build opencarrier，需要先改 hook 见下）
- 本地三连绿：`cargo build --workspace --lib && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
- 服务器 PG 16 active（已确认 `systemctl is-active postgresql` = active）

## 1. 创建 PG 数据库

```bash
ssh 86quan 'sudo -u postgres createdb aginx_memory 2>/dev/null; sudo -u postgres psql -c "CREATE USER ubuntu 2>/dev/null" ; sudo -u postgres psql -c "GRANT ALL ON DATABASE aginx_memory TO ubuntu"'
```

`deploy/aginx-memory.service` 里 `DATABASE_URL=postgres:///aginx_memory`（peer auth，ubuntu 用户）。

## 2. 安装 aginx-memory.service

```bash
# 把仓内 deploy/aginx-memory.service 装到 systemd（hook 还没自动装，先手动）
scp deploy/aginx-memory.service 86quan:/tmp/aginx-memory.service
ssh 86quan 'sudo mv /tmp/aginx-memory.service /etc/systemd/system/aginx-memory.service && sudo systemctl daemon-reload'
```

## 3. 改 post-receive hook（build aginx-memory 两个 bin）

编辑 `/data/git/opencarrier-workspace.git/hooks/post-receive`，在 build opencarrier 之后加：

```bash
echo ">>> Building aginx-memory (daemon + migrate)..."
cargo build --release --bin aginx-memory --bin aginx-memory-migrate 2>&1
AginxBinary="/home/ubuntu/.opencarrier/aginx-memory"
cp "target/release/aginx-memory" "${AginxBinary}.new"
mv "${AginxBinary}.new" "$AginxBinary"
chmod +x "$AginxBinary"
cp "target/release/aginx-memory-migrate" "/home/ubuntu/.opencarrier/aginx-memory-migrate"
chmod +x "/home/ubuntu/.opencarrier/aginx-memory-migrate"
```

并把结尾的 restart 段改成 restart **两个** service（aginx-memory 先，opencarrier 后）：

```bash
sudo systemctl restart aginx-memory 2>/dev/null || sudo systemctl start aginx-memory
sudo systemctl restart opencarrier
```

> hook 是服务器侧文件（不在仓内），按 CLAUDE.md「never edit files directly on server」——hook 本身是 deploy 管线的例外（它是 deploy 的实现），直接 SSH 编辑。

## 4. 推代码触发 build

```bash
git push deploy main
# 观察 hook 输出：build opencarrier + aginx-memory + aginx-memory-migrate，restart 两 service
```

build 完后 aginx-memory.service 已起（WORKER_COUNT=0 queue-only，scheduler off）。验证：

```bash
ssh 86quan 'curl -s http://127.0.0.1:4300/health'   # -> ok
ssh 86quan 'systemctl is-active aginx-memory'        # -> active
ssh 86quan 'journalctl -u aginx-memory --no-pager | tail'  # PG migrations applied
```

**此时 opencarrier 仍用进程内 SQLite**（AGINXMEMORY_URL 未设，make_memory_handle 回退 MemorySubstrateHandle）。aginx-memory 空跑验证自身健康。

## 5. 切换：迁移数据 + 设 env（有停机）

> **实际 db 路径是 `~/.opencarrier/data/opencarrier.db`（164M），不是 `~/.opencarrier/opencarrier.db`（0 字节旧文件）。**
> **不要 rename 整个 db** -- opencarrier.db 还含 sessions/agents/cron（运行时表，留进程内）。迁移只读记忆表，db 原地保留。
> **DATABASE_URL 要带 `?host=/var/run/postgresql`**（tokio-postgres peer auth 走 unix socket，`postgres:///db` 缺 host 会报 "both host and hostaddr are missing"）。

```bash
ssh 86quan '
set -e
# 5a. 停 opencarrier（停写，保证迁移快照一致）
sudo systemctl stop opencarrier

# 5b. 迁移线上数据（只读开 data/opencarrier.db -> PG）
~/.opencarrier/aginx-memory-migrate \
  --sqlite ~/.opencarrier/data/opencarrier.db \
  --pg "postgres:///aginx_memory?host=/var/run/postgresql" \
  --content-src ~/.opencarrier/memory_tree/content \
  --content-dst ~/.opencarrier/memory_tree/content

# 5c. 对账：每表行数 PG vs SQLite
for t in kv_store kv_history mem_tree_chunks mem_tree_trees mem_tree_score mem_tree_entity_index mem_tree_entity_hotness; do
  pg=$(psql -t -d aginx_memory -c "SELECT count(*) FROM $t")
  sq=$(sqlite3 ~/.opencarrier/data/opencarrier.db "SELECT count(*) FROM $t")
  echo "$t: pg=$pg sqlite=$sq"
done
# mem_tree_jobs 应为 0（迁移后清空，历史 chunk 已在 chunks 表）

# 5d. 设 opencarrier env（指向 aginx-memory）
grep -q AGINXMEMORY_URL ~/.opencarrier/.env || echo "AGINXMEMORY_URL=http://127.0.0.1:4300" >> ~/.opencarrier/.env

# 5e. 起 opencarrier
sudo systemctl start opencarrier
'
```

> **不 rename opencarrier.db**。记忆表在 SQLite 里变 stale（AGINXMEMORY_URL 开后 opencarrier 不读它们）但无害；运行时表（sessions/agents/cron）继续用。

## 6. 观察期（24h，WORKER_COUNT=0）

- opencarrier 日志无 memory error，发消息 -> kv/tree 走 HTTP（看 aginx-memory journal 有请求）
- aginx-memory `SELECT count(*) FROM mem_tree_jobs` 增长（ingest 入队 ExtractChunk，但 worker=0 不消费）—— 正常
- 数据迁移对账每表 pg==sqlite

## 7. 分级开 worker（plan 风险#1）

确认 24h 无异常后，逐步开消费：

```bash
ssh 86quan 'sudo systemctl edit aginx-memory'
# 在 override 里加（或改 .env）：
#   [Service]
#   Environment=AGINX_MEMORY_WORKER_COUNT=1
# 然后 sudo systemctl restart aginx-memory
# 观察 worker 跑 extract_chunk->append_buffer->seal->topic_route，无 panic、无双 seal
# 稳定后再 -> 4
```

digest 验证：手动触发一次 `trigger_digest`（或临时 AGINX_MEMORY_SCHEDULER=on 跑一轮）确认 global tree digest 正常，再常驻开 scheduler。

## 8. 清理（稳定后）

- 删 `~/.opencarrier/opencarrier.db.migrated-bak`
- 阶段 7 清理 memory crate 死代码（jobs/worker+scheduler 未被 aginx-memory 用、`impl MemoryHandle for CarrierKernel`）

## 回滚

迁移期任何阶段出问题：

```bash
ssh 86quan '
sudo systemctl stop opencarrier
# 去掉 env 开关
sed -i "/AGINXMEMORY_URL/d" ~/.opencarrier/.env
# 恢复原库（迁移是只读 opencarrier.db，原库未被改；若已 rename 则恢复）
[ -f ~/.opencarrier/opencarrier.db.migrated-bak ] && mv ~/.opencarrier/opencarrier.db.migrated-bak ~/.opencarrier/opencarrier.db
sudo systemctl start opencarrier
'
# opencarrier 回退进程内 SQLite；aginx-memory 可停或留空跑
```

`AGINXMEMORY_URL` 一去，opencarrier 的 `make_memory_handle` 立即回退 `MemorySubstrateHandle`，6 注入点 + 5 直接调用点全回进程内。**回滚是无损的**（迁移是只读源库 + 新写 PG，回退后从 SQLite 续写）。

## 风险

1. **tree jobs 首次激活**（最高危，已缓解）：seal/topic_route/digest 从未在进程内跑过，aginx-memory worker 是首跑。阶段2 PG 测试覆盖三路径，阶段4 worker_count=1 smoke 验证 extract->append_buffer->topic_route 闭环。WORKER_COUNT 默认 0（入队不消费）+ 分级 0->1->4。
2. **数据迁移对账**：kv value BLOB->JSONB 解析失败存 NULL（不丢行）；每表行数 pg==sqlite 强校验。
3. **block_in_place**：opencarrier multi-thread runtime，5 直��调用点 + HttpMemoryHandle kv 桥接安全（集成测试验证）。
