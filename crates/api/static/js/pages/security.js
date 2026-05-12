// OpenCarrier Security Page — Core protections, configurable controls, audit, credentials
'use strict';

function securityPage() {
  return {
    loading: true,
    loadError: '',
    securityData: null,
    verifyingChain: false,
    chainResult: null,
    credSaving: false,
    credCurrentPass: '',
    credNewUser: '',
    credNewPass: '',
    coreFeatures: [
      { name: '路径遍历防护', key: 'path_traversal', description: '阻止所有文件操作中的目录逃逸攻击。' },
      { name: 'SSRF 防护', key: 'ssrf_protection', description: '阻止对私有 IP 和云元数据端点的出站请求。' },
      { name: '基于能力的访问控制', key: 'capability_system', description: '默认拒绝的权限系统。' },
      { name: '子进程环境隔离', key: 'subprocess_isolation', description: '子进程仅继承安全的环境变量。' },
      { name: '安全响应头', key: 'security_headers', description: '每个 HTTP 响应包含 CSP、X-Frame-Options 等安全头。' }
    ],

    async loadSecurity() {
      this.loading = true;
      this.loadError = '';
      try {
        this.securityData = await OpenCarrierAPI.get('/api/security');
      } catch(e) {
        this.loadError = e.message || '加载安全配置失败';
        this.securityData = null;
      }
      this.loading = false;
    },

    isActive(key) {
      if (!this.securityData) return true;
      var core = this.securityData.core_protections || {};
      return core[key] !== undefined ? core[key] : true;
    },

    async verifyAuditChain() {
      this.verifyingChain = true;
      this.chainResult = null;
      try {
        var res = await OpenCarrierAPI.get('/api/audit/verify');
        this.chainResult = res;
      } catch(e) {
        this.chainResult = { valid: false, error: e.message };
      }
      this.verifyingChain = false;
    },

    async saveCredentials() {
      if (!this.credCurrentPass) { OpenCarrierToast.error('请输入当前密码'); return; }
      if (!this.credNewUser && !this.credNewPass) { OpenCarrierToast.error('请输入新用户名或新密码'); return; }
      if (this.credNewPass && this.credNewPass.length < 6) { OpenCarrierToast.error('新密码至少 6 位'); return; }
      this.credSaving = true;
      try {
        var data = await OpenCarrierAPI.post('/api/auth/change-credentials', {
          current_password: this.credCurrentPass,
          new_username: this.credNewUser,
          new_password: this.credNewPass
        });
        if (data.token) {
          document.cookie = 'opencarrier_session=' + data.token + '; Path=/; SameSite=Strict; Max-Age=604800';
        }
        if (Alpine.store('app')) Alpine.store('app').sessionUser = data.username;
        this.credCurrentPass = '';
        this.credNewUser = '';
        this.credNewPass = '';
        OpenCarrierToast.success('凭证已更新');
      } catch(e) { OpenCarrierToast.error('更新失败: ' + (e.message || e)); }
      this.credSaving = false;
    },
  };
}
