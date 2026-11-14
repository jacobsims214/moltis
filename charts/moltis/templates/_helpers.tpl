{{/*
Chart-level names.
*/}}
{{- define "moltis.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "moltis.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}

{{/*
Resolve every enabled instance: defaults deep-merged with the instance's own
values. Returns YAML: a list of {name, fullname, cfg}.
*/}}
{{- define "moltis.instances" -}}
{{- $out := list }}
{{- range $name, $inst := .Values.instances }}
{{- $inst = default dict $inst }}
{{- if not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" $name) }}
{{- fail (printf "instance name %q must be a lowercase DNS label" $name) }}
{{- end }}
{{- $cfg := mergeOverwrite (deepCopy $.Values.defaults) (deepCopy $inst) }}
{{- $full := printf "%s-%s" $.Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- $out = append $out (dict "name" $name "fullname" $full "enabled" (ne (toString (get $inst "enabled")) "false") "cfg" $cfg) }}
{{- end }}
{{- toYaml $out }}
{{- end }}

{{- define "moltis.labels" -}}
helm.sh/chart: {{ include "moltis.chart" .root }}
app.kubernetes.io/name: moltis
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .i.name }}
app.kubernetes.io/version: {{ default .root.Chart.AppVersion .root.Values.image.tag | quote }}
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
{{- end }}

{{- define "moltis.selectorLabels" -}}
app.kubernetes.io/name: moltis
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .i.name }}
{{- end }}

{{- define "moltis.passwordSecretName" -}}
{{- if .i.cfg.password.existingSecret }}{{ .i.cfg.password.existingSecret }}{{ else }}{{ .i.fullname }}-password{{ end }}
{{- end }}

{{- define "moltis.claimName" -}}
{{- if .i.cfg.persistence.existingClaim }}{{ .i.cfg.persistence.existingClaim }}{{ else }}{{ .i.fullname }}{{ end }}
{{- end }}

{{/*
Seed moltis.toml for one instance.
*/}}
{{- define "moltis.configToml" -}}
{{- $c := .i.cfg -}}
# Seeded by the moltis Helm chart. Moltis edits this file from the UI; it is
# only rewritten from the chart when configOverwrite is true.

[server]
bind = "0.0.0.0"
port = {{ int $c.service.port }}
terminal_enabled = {{ $c.terminal }}

[tls]
enabled = false

[tools.exec.sandbox]
backend = {{ $c.sandboxBackend | quote }}
{{- if or $c.displayName $c.timezone }}

[user]
{{- with $c.displayName }}
name = {{ . | quote }}
{{- end }}
{{- with $c.timezone }}
timezone = {{ . | quote }}
{{- end }}
{{- end }}
{{- if or $c.agentName $c.agentEmoji }}

[identity]
{{- with $c.agentName }}
name = {{ . | quote }}
{{- end }}
{{- with $c.agentEmoji }}
emoji = {{ . | quote }}
{{- end }}
{{- end }}
{{- with $c.llm }}
{{- if .baseUrl }}
{{- $provider := printf "custom-%s" .name }}

[providers]
{{- if .exclusive }}
offered = [{{ $provider | quote }}]
{{- end }}

[providers.{{ $provider }}]
enabled = true
base_url = {{ .baseUrl | quote }}
{{- if not .apiKeySecret.name }}
api_key = {{ .apiKey | quote }}
{{- else }}
api_key = "${MOLTIS_LLM_API_KEY}"
{{- end }}
tool_mode = {{ .toolMode | quote }}
{{- with .models }}
models = {{ toJson . }}
{{- end }}
{{- range $model, $ctx := .contextWindows }}

[providers.{{ $provider }}.model_overrides.{{ $model | quote }}]
context_window = {{ int $ctx }}
{{- end }}
{{- with .models }}

[chat]
priority_models = {{ toJson . }}
{{- end }}
{{- end }}
{{- end }}
{{- with $c.extraConfigToml }}

{{ . }}
{{- end }}
{{- end }}

{{- define "moltis.volumeMounts" -}}
- name: state
  mountPath: /home/moltis/.config/moltis
  subPath: config
- name: state
  mountPath: /home/moltis/.moltis
  subPath: data
- name: state
  mountPath: /home/moltis/.npm
  subPath: npm
{{- end }}
