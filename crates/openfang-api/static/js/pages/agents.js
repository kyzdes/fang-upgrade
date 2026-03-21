// OpenFang Agents Page — Multi-step spawn wizard, detail view with tabs, file editor, personality presets
'use strict';

/** Escape a string for use inside TOML triple-quoted strings ("""\n...\n""").
 *  Backslashes are escaped, and runs of 3+ consecutive double-quotes are
 *  broken up so the TOML parser never sees an unintended closing delimiter.
 */
function tomlMultilineEscape(s) {
  return s.replace(/\\/g, '\\\\').replace(/"""/g, '""\\"');
}

/** Escape a string for use inside a TOML basic (single-line) string ("...").
 *  Backslashes, double-quotes, and common control chars are escaped.
 */
function tomlBasicEscape(s) {
  return s.replace(/\\/g, '\\\\').replace(/"/g, '\"').replace(/\n/g, '\\n').replace(/\r/g, '\\r').replace(/\t/g, '\\t');
}

function agentsPage() {
  return {
    tab: 'agents',
    activeChatAgent: null,
    // -- Agents state --
    showSpawnModal: false,
    showDetailModal: false,
    detailAgent: null,
    spawnMode: 'wizard',
    spawning: false,
    spawnToml: '',
    filterState: 'all',
    loading: true,
    loadError: '',
    spawnForm: {
      name: '',
      provider: 'groq',
      model: 'llama-3.3-70b-versatile',
      systemPrompt: 'You are a helpful assistant.',
      profile: 'full',
      caps: { memory_read: true, memory_write: true, network: false, shell: false, agent_spawn: false }
    },

    // -- Multi-step wizard state --
    spawnStep: 1,
    spawnIdentity: { emoji: '', color: '#FF5C00', archetype: '' },
    selectedPreset: '',
    soulContent: '',
    emojiOptions: [
      '\u{1F916}', '\u{1F4BB}', '\u{1F50D}', '\u{270D}\uFE0F', '\u{1F4CA}', '\u{1F6E0}\uFE0F',
      '\u{1F4AC}', '\u{1F393}', '\u{1F310}', '\u{1F512}', '\u{26A1}', '\u{1F680}',
      '\u{1F9EA}', '\u{1F3AF}', '\u{1F4D6}', '\u{1F9D1}\u200D\u{1F4BB}', '\u{1F4E7}', '\u{1F3E2}',
      '\u{2764}\uFE0F', '\u{1F31F}', '\u{1F527}', '\u{1F4DD}', '\u{1F4A1}', '\u{1F3A8}'
    ],
    archetypeOptions: ['Assistant', 'Researcher', 'Coder', 'Writer', 'DevOps', 'Support', 'Analyst', 'Custom'],
    personalityPresets: [
      { id: 'professional', label: 'Professional', soul: 'Communicate in a clear, professional tone. Be direct and structured. Use formal language and data-driven reasoning. Prioritize accuracy over personality.' },
      { id: 'friendly', label: 'Friendly', soul: 'Be warm, approachable, and conversational. Use casual language and show genuine interest in the user. Add personality to your responses while staying helpful.' },
      { id: 'technical', label: 'Technical', soul: 'Focus on technical accuracy and depth. Use precise terminology. Show your work and reasoning. Prefer code examples and structured explanations.' },
      { id: 'creative', label: 'Creative', soul: 'Be imaginative and expressive. Use vivid language, analogies, and unexpected connections. Encourage creative thinking and explore multiple perspectives.' },
      { id: 'concise', label: 'Concise', soul: 'Be extremely brief and to the point. No filler, no pleasantries. Answer in the fewest words possible while remaining accurate and complete.' },
      { id: 'mentor', label: 'Mentor', soul: 'Be patient and encouraging like a great teacher. Break down complex topics step by step. Ask guiding questions. Celebrate progress and build confidence.' }
    ],

    // -- Detail modal tabs --
    detailTab: 'info',
    agentFiles: [],
    editingFile: null,
    fileContent: '',
    fileSaving: false,
    filesLoading: false,
    configForm: {},
    configSaving: false,
    // -- Tool filters --
    toolFilters: { tool_allowlist: [], tool_blocklist: [] },
    toolFiltersLoading: false,
    newAllowTool: '',
    newBlockTool: '',
    // -- Model switch --
    editingModel: false,
    newModelValue: '',
    modelSaving: false,
    // -- Fallback chain --
    editingFallback: false,
    newFallbackValue: '',

    // -- Templates state --
    tplTemplates: [],
    tplProviders: [],
    tplLoading: false,
    tplLoadError: '',
    selectedCategory: 'All',
    searchQuery: '',
    templatesLoading: true,
    templatesError: null,

    builtinTemplates: [],

    // Load templates from API
    created() {
      this.loadTemplates();
    },

    methods: {
      loadTemplates() {
        this.templatesLoading = true;
        this.templatesError = null;
        
        fetch('/api/templates')
          .then(response => {
            if (!response.ok) throw new Error('Failed to load templates');
            return response.json();
          })
          .then(data => {
            // Map API data to expected format
            this.builtinTemplates = data.templates.map(template => ({
              name: template.name,
              description: template.description,
              category: this.getTemplateCategory(template.name),
              provider: 'groq',
              model: 'llama-3.3-70b-versatile',
              profile: 'full',
              system_prompt: 'You are a helpful, friendly assistant. Provide clear, accurate, and concise responses. Ask clarifying questions when needed.'
            }));
          })
          .catch(error => {
            console.error('Error loading templates:', error);
            this.templatesError = 'Failed to load agent templates';
            // Fallback to hardcoded templates if available
            this.builtinTemplates = this.getFallbackTemplates();
          })
          .finally(() => {
            this.templatesLoading = false;
          });
      },
      
      getTemplateCategory(name) {
        // Map template names to categories
        const categoryMap = {
          'hello-world': 'General',
          'assistant': 'General',
          'researcher': 'Research',
          'coder': 'Development',
          'writer': 'Writing',
          'analyst': 'Analysis',
          'ops': 'Operations',
          'planner': 'Business',
          'debugger': 'Development',
          'devops-lead': 'Development',
          'code-reviewer': 'Development',
          'data-scientist': 'Analysis',
          'test-engineer': 'Development',
          'legal-assistant': 'Business',
          'doc-writer': 'Writing',
          'email-assistant': 'Business',
          'social-media': 'Business',
          'customer-support': 'Business',
          'sales-assistant': 'Business',
          'recruiter': 'Business',
          'meeting-assistant': 'Business',
          'translator': 'General',
          'tutor': 'General',
          'health-tracker': 'General',
          'personal-finance': 'General',
          'travel-planner': 'General',
          'home-automation': 'General',
          'architect': 'Development',
          'security-auditor': 'Development'
        };
        return categoryMap[name] || 'General';
      },
      
      getFallbackTemplates() {
        // Return hardcoded templates as fallback
        return [
          {
            name: 'General Assistant',
            description: 'A versatile conversational agent that can help with everyday tasks, answer questions, and provide recommendations.',
            category: 'General',
            provider: 'groq',
            model: 'llama-3.3-70b-versatile',
            profile: 'full',
            system_prompt: 'You are a helpful, friendly assistant. Provide clear, accurate, and concise responses. Ask clarifying questions when needed.'
          },
          {
            name: 'Code Helper',
            description: 'A programming-focused agent that writes, reviews, and debugs code across multiple languages.',
            category: 'Development',
            provider: 'groq',
            model: 'llama-3.3-70b-versatile',
            profile: 'coding',
            system_prompt: 'You are an expert programmer. Help users write clean, efficient code. Explain your reasoning. Follow best practices and conventions for the language being used.'
          },
          {
            name: 'Researcher',
            description: 'An analytical agent that breaks down complex topics, synthesizes information, and provides cited summaries.',
            category: 'Research',
            provider: 'groq',
            model: 'llama-3.3-70b-versatile',
            profile: 'research',
            system_prompt: 'You are a research analyst. Break down complex topics into clear explanations. Provide structured analysis with key findings. Cite sources when available.'
          },
          {
            name: 'Writer',
            description: 'A creative writing agent that helps with drafting, editing, and improving written content of all kinds.',
            category: 'Writing',
            provider: 'groq',
            model: 'llama-3.3-70b-versatile',
            profile: 'full',
            system_prompt: 'You are a skilled writer and editor. Help users create polished content. Adapt your tone and style to match the intended audience. Offer constructive suggestions for improvement.'
          },
          {
            name: 'Data Analyst',
            description: 'A data-focused agent that helps analyze datasets, create queries, and interpret statistical results.',
            category: 'Development',
            provider: 'groq',
            model: 'llama-3.3-70b-versatile',
            profile: 'coding',
            system_prompt: 'You are a data analysis expert. Help users understand their data, write SQL/Python queries, and interpret results. Present findings clearly with actionable insights.'
          },
          {
            name: 'DevOps Engineer',
            description: 'A systems-focused agent for CI/CD, infrastructure, Docker, and deployment troubleshooting.',
            category: 'Development',
            provider: 'groq',
            model: 'llama-3.3-70b-versatile',
            profile: 'automation',
            system_prompt: 'You are a DevOps engineer. Help with CI/CD pipelines, Docker, Kubernetes, infrastructure as code, and deployment. Prioritize reliability and security.'
          }
        ];
      }
    },

    // ── Profile Descriptions ──
    profileDescriptions: {
      minimal: { label: 'Minimal', desc: 'Read-only file access' },
      coding: { label: 'Coding', desc: 'Files + shell + web fetch' },
      research: { label: 'Research', desc: 'Web search + file read/write' },
      messaging: { label: 'Messaging', desc: 'Agents + memory access' },
      automation: { label: 'Automation', desc: 'All tools except custom' },
      balanced: { label: 'Balanced', desc: 'General-purpose tool set' },
      precise: { label: 'Precise', desc: 'Focused tool set for accuracy' },
      creative: { label: 'Creative', desc: 'Full tools with creative emphasis' },
      full: { label: 'Full', desc: 'All 35+ tools' }
    },
    profileInfo: function(name) {
      return this.profileDescriptions[name] || { label: name, desc: '' };
    },

    // ── Tool Preview in Spawn Modal ──
    spawnProfiles: [],
    spawnProfilesLoaded: false,
    async loadSpawnProfiles() {
      if (this.spawnProfilesLoaded) return;
      try {
        var data = await OpenFangAPI.get('/api/profiles');
        this.spawnProfiles = data.profiles || [];
        this.spawnProfilesLoaded = true;
      } catch(e) { this.spawnProfiles = []; }
    },
    get selectedProfileTools() {
      var pname = this.spawnForm.profile;
      var match = this.spawnProfiles.find(function(p) { return p.name === pname; });
      if (match && match.tools) return match.tools.slice(0, 15);
      return [];
    },

    get agents() { return Alpine.store('app').agents; },

    get filteredAgents() {
      var f = this.filterState;
      if (f === 'all') return this.agents;
      return this.agents.filter(function(a) { return a.state.toLowerCase() === f; });
    },

    get runningCount() {
      return this.agents.filter(function(a) { return a.state === 'Running'; }).length;
    },

    get stoppedCount() {
      return this.agents.filter(function(a) { return a.state !== 'Running'; }).length;
    },

    // -- Templates computed --
    get categories() {
      var cats = { 'All': true };
      this.builtinTemplates.forEach(function(t) { cats[t.category] = true; });
      this.tplTemplates.forEach(function(t) { if (t.category) cats[t.category] = true; });
      return Object.keys(cats);
    },

    get filteredBuiltins() {
      var self = this;
      return this.builtinTemplates.filter(function(t) {
        if (self.selectedCategory !== 'All' && t.category !== self.selectedCategory) return false;
        if (self.searchQuery) {
          var q = self.searchQuery.toLowerCase();
          if (t.name.toLowerCase().indexOf(q) === -1 &&
              t.description.toLowerCase().indexOf(q) === -1) return false;
        }
        return true;
      });
    },

    get filteredCustom() {
      var self = this;
      return this.tplTemplates.filter(function(t) {
        if (self.searchQuery) {
          var q = self.searchQuery.toLowerCase();
          if ((t.name || '').toLowerCase().indexOf(q) === -1 &&
              (t.description || '').toLowerCase().indexOf(q) === -1) return false;
        }
        return true;
      });
    },

    isProviderConfigured(providerName) {
      if (!providerName) return true;
      var p = this.tplProviders.find(function(pr) { return pr.id === providerName; });
      return p ? p.auth_status === 'configured' : false;
    },

    // ... (rest of the file remains unchanged)
  };
}