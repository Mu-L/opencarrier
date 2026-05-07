// OpenCarrier Brain Page — Config, Brain config, Provider keys, Endpoints, Modalities
'use strict';

function brainPage() {
  return {
    loading: true,
    loadError: '',
    providerKeys: [],
    providerKeyInputs: {},
    providerKeySaving: {},
    brainInfo: null,
    brainStatus: null,
    showAddEndpoint: false,
    showAddModality: false,
    newEndpoint: { name: '', provider: '', model: '', base_url: '', format: 'openai' },
    newModality: { name: '', primary: '', fallbacks: '' },

    async loadBrain() {
      this.loading = true;
      this.loadError = '';
      try {
        await Promise.all([this.loadProviderKeys(), this.loadBrainInfo(), this.loadBrainStatus()]);
      } catch(e) { this.loadError = e.message || '加载失败'; }
      this.loading = false;
    },

    async loadBrainInfo() {
      try {
        this.brainInfo = await OpenCarrierAPI.get('/api/brain');
      } catch(e) { this.brainInfo = null; }
    },

    async loadBrainStatus() {
      try {
        this.brainStatus = await OpenCarrierAPI.get('/api/brain/status');
      } catch(e) { this.brainStatus = null; }
    },

    get endpointsList() {
      if (!this.brainInfo || !this.brainInfo.endpoints) return [];
      return Object.entries(this.brainInfo.endpoints).map(function([name, ep]) {
        return Object.assign({ name: name }, ep);
      }).sort(function(a, b) { return a.name.localeCompare(b.name); });
    },

    get modalitiesList() {
      if (!this.brainInfo || !this.brainInfo.modalities) return [];
      return Object.entries(this.brainInfo.modalities).map(function([name, m]) {
        return Object.assign({ name: name }, m);
      }).sort(function(a, b) { return a.name.localeCompare(b.name); });
    },

    getEndpointHealth(name) {
      if (!this.brainStatus || !this.brainStatus.endpoints) return null;
      return this.brainStatus.endpoints.find(function(e) { return e.endpoint === name; });
    },

    async reloadBrain() {
      try {
        await OpenCarrierAPI.post('/api/brain/reload', {});
        await Promise.all([this.loadBrainInfo(), this.loadBrainStatus()]);
        OpenCarrierToast.success('Brain 已重新加载');
      } catch(e) { OpenCarrierToast.error('重载失败: ' + (e.message || e)); }
    },

    async deleteEndpoint(name) {
      if (!confirm('确定删除端点 ' + name + ' 吗？')) return;
      try {
        await OpenCarrierAPI.del('/api/brain/endpoints/' + encodeURIComponent(name));
        await Promise.all([this.loadBrainInfo(), this.loadBrainStatus()]);
        OpenCarrierToast.success('已删除端点 ' + name);
      } catch(e) { OpenCarrierToast.error('删除失败: ' + (e.message || e)); }
    },

    async addEndpoint() {
      var ep = this.newEndpoint;
      if (!ep.name || !ep.provider || !ep.model || !ep.base_url) {
        OpenCarrierToast.error('请填写所有必填字段');
        return;
      }
      try {
        await OpenCarrierAPI.put('/api/brain/endpoints/' + encodeURIComponent(ep.name), {
          provider: ep.provider, model: ep.model, base_url: ep.base_url, format: ep.format
        });
        this.showAddEndpoint = false;
        this.newEndpoint = { name: '', provider: '', model: '', base_url: '', format: 'openai' };
        await Promise.all([this.loadBrainInfo(), this.loadBrainStatus()]);
        OpenCarrierToast.success('已添加端点 ' + ep.name);
      } catch(e) { OpenCarrierToast.error('添加失败: ' + (e.message || e)); }
    },

    async deleteModality(name) {
      if (!confirm('确定删除模态 ' + name + ' 吗？')) return;
      try {
        await OpenCarrierAPI.del('/api/brain/modalities/' + encodeURIComponent(name));
        await this.loadBrainInfo();
        OpenCarrierToast.success('已删除模态 ' + name);
      } catch(e) { OpenCarrierToast.error('删除失败: ' + (e.message || e)); }
    },

    async addModality() {
      var m = this.newModality;
      if (!m.name || !m.primary) {
        OpenCarrierToast.error('请填写名称和主端点');
        return;
      }
      var fallbacks = m.fallbacks ? m.fallbacks.split(',').map(function(s) { return s.trim(); }).filter(Boolean) : [];
      try {
        await OpenCarrierAPI.put('/api/brain/modalities/' + encodeURIComponent(m.name), {
          primary: m.primary, fallbacks: fallbacks
        });
        this.showAddModality = false;
        this.newModality = { name: '', primary: '', fallbacks: '' };
        await this.loadBrainInfo();
        OpenCarrierToast.success('已添加模态 ' + m.name);
      } catch(e) { OpenCarrierToast.error('添加失败: ' + (e.message || e)); }
    },

    async loadProviderKeys() {
      try {
        var data = await OpenCarrierAPI.get('/api/providers/keys');
        this.providerKeys = data.providers || [];
        this.providerKeyInputs = {};
      } catch(e) { this.providerKeys = []; }
    },

    async saveProviderKey(name) {
      var p = this.providerKeys.find(function(x){return x.name===name});
      if (p && p.auth_type === 'jwt') { return this.saveProviderKeyJwt(name); }
      var key = (this.providerKeyInputs[name] || '').trim();
      if (!key) { OpenCarrierToast.error('API 密钥不能为空'); return; }
      this.providerKeySaving[name] = true;
      try {
        await OpenCarrierAPI.post('/api/providers/' + name + '/key', { key: key });
        await this.loadProviderKeys();
        OpenCarrierToast.success('已保存 ' + name + ' 的 API 密钥');
      } catch(e) { OpenCarrierToast.error('保存密钥失败: ' + (e.message || e)); }
      this.providerKeySaving[name] = false;
    },

    async saveProviderKeyJwt(name) {
      var p = this.providerKeys.find(function(x){return x.name===name});
      if (!p) return;
      var params = {};
      var hasValue = false;
      (p.params || []).forEach(function(param) {
        var val = (this.providerKeyInputs[name + '_' + param.name] || '').trim();
        if (val) { params[param.name] = val; hasValue = true; }
      }.bind(this));
      if (!hasValue) { OpenCarrierToast.error('请至少填写一项凭证'); return; }
      this.providerKeySaving[name] = true;
      try {
        await OpenCarrierAPI.post('/api/providers/' + name + '/key', { params: params });
        await this.loadProviderKeys();
        OpenCarrierToast.success('已保存 ' + name + ' 的凭证');
      } catch(e) { OpenCarrierToast.error('保存凭证失败: ' + (e.message || e)); }
      this.providerKeySaving[name] = false;
    },

    async deleteProviderKey(name) {
      if (!confirm('确定删除 ' + name + ' 的凭证吗？')) return;
      try {
        await OpenCarrierAPI.del('/api/providers/' + name + '/key');
        await this.loadProviderKeys();
        OpenCarrierToast.success('已删除 ' + name + ' 的凭证');
      } catch(e) { OpenCarrierToast.error('删除凭证失败: ' + (e.message || e)); }
    },

  };
}
