#!/usr/bin/env bash
#
# NexusNet systemd 卸载脚本（幂等）
#
# 注意：不会删除节点身份（/var/lib/nexusnet/keypair.bin）与日志归档（/var/log/nexusnet），
# 以免丢失 PeerId 与历史记录。如需彻底清理请手动执行。
#
set -euo pipefail

UNIT_NAME="nexusnet"
SERVICE_USER="nexusnet"

CONFIG_FILE="/etc/${UNIT_NAME}/config.toml"
DATA_DIR="/var/lib/${UNIT_NAME}"
LOG_DIR="/var/log/${UNIT_NAME}"

log() { printf '[uninstall] %s\n' "$*"; }

# 1. 停止并禁用服务
log "停止并禁用服务 ${UNIT_NAME}..."
systemctl disable --now "${UNIT_NAME}" 2>/dev/null || true

# 2. 移除单元与配套配置文件
rm -f "/etc/systemd/system/${UNIT_NAME}.service"
rm -f "/etc/sysusers.d/${UNIT_NAME}.conf"
rm -f "/etc/tmpfiles.d/${UNIT_NAME}.conf"
systemctl daemon-reload
log "已移除 systemd 单元与配置文件"

# 3. 移除二进制
rm -f "/usr/local/bin/${UNIT_NAME}"
log "已移除二进制 /usr/local/bin/${UNIT_NAME}"

# 4. 移除专用用户（保留数据）
if id "${SERVICE_USER}" >/dev/null 2>&1; then
    userdel "${SERVICE_USER}" 2>/dev/null || true
    log "已移除用户 ${SERVICE_USER}"
fi

# 5. 提示保留数据
echo
echo "==== 保留的数据（如需清理请手动处理） ===="
echo "  节点身份: ${DATA_DIR}/keypair.bin   (删除将导致 PeerId 变更)"
echo "  配置文件: ${CONFIG_FILE}"
echo "  日志归档: ${LOG_DIR}"
echo
log "卸载完成。"
