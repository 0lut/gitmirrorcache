{{/*
Expand the name of the chart.
*/}}
{{- define "gitmirrorcache.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "gitmirrorcache.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Chart label.
*/}}
{{- define "gitmirrorcache.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels.
*/}}
{{- define "gitmirrorcache.labels" -}}
helm.sh/chart: {{ include "gitmirrorcache.chart" . }}
{{ include "gitmirrorcache.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels.
*/}}
{{- define "gitmirrorcache.selectorLabels" -}}
app.kubernetes.io/name: {{ include "gitmirrorcache.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Service account name.
*/}}
{{- define "gitmirrorcache.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "gitmirrorcache.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Image reference.
*/}}
{{- define "gitmirrorcache.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}

{{/*
Shared GIT_CACHE_* environment variables used by both the server and the
compaction CronJob. Takes a dict: "ctx" (the root context) and an optional
"cacheRoot" override so the CronJob can use a scratch directory. GIT_CACHE_CONFIG
is deliberately not set here: when it is set the application reads the entire
config from the TOML file and ignores env vars, so only the server (which
mounts the ConfigMap) opts into it.
*/}}
{{- define "gitmirrorcache.env" -}}
{{- $ctx := .ctx -}}
{{- $cacheRoot := default $ctx.Values.config.cacheRoot .cacheRoot -}}
{{- with $ctx }}
- name: GIT_CACHE_BIND_ADDR
  value: {{ .Values.config.bindAddr | quote }}
- name: GIT_CACHE_ROOT
  value: {{ $cacheRoot | quote }}
- name: GIT_CACHE_GIT_BINARY
  value: {{ .Values.config.gitBinary | quote }}
- name: GIT_CACHE_GIT_TIMEOUT_SECONDS
  value: {{ .Values.config.gitTimeoutSeconds | int64 | quote }}
- name: GIT_CACHE_MAX_GIT_OUTPUT_BYTES
  value: {{ .Values.config.maxGitOutputBytes | int64 | quote }}
- name: GIT_CACHE_RATE_LIMIT_PER_MINUTE
  value: {{ .Values.config.rateLimitPerMinute | int64 | quote }}
- name: GIT_CACHE_ALLOWED_UPSTREAM_HOSTS
  value: {{ join "," .Values.config.allowedUpstreamHosts | quote }}
- name: GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES
  value: {{ .Values.config.maxConcurrentGitProcesses | int64 | quote }}
- name: GIT_CACHE_DISK_QUOTA_BYTES
  value: {{ .Values.config.disk.quotaBytes | int64 | quote }}
- name: GIT_CACHE_DISK_MIN_FREE_BYTES
  value: {{ .Values.config.disk.minFreeBytes | int64 | quote }}
- name: GIT_CACHE_GIT_REMOTE_ENABLED
  value: {{ .Values.config.gitRemote.enabled | quote }}
- name: GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD
  value: {{ .Values.config.compaction.chainDepthThreshold | int64 | quote }}
- name: GIT_CACHE_COMPACTION_INLINE
  value: {{ .Values.config.compaction.inline | quote }}
- name: GIT_CACHE_SHUTDOWN_READINESS_DELAY_SECONDS
  value: {{ .Values.config.shutdown.readinessDelaySeconds | int64 | quote }}
- name: GIT_CACHE_SHUTDOWN_DRAIN_TIMEOUT_SECONDS
  value: {{ .Values.config.shutdown.drainTimeoutSeconds | int64 | quote }}
- name: RUST_LOG
  value: {{ .Values.config.logLevel | quote }}
{{- if eq .Values.config.objectStore.kind "s3" }}
- name: GIT_CACHE_OBJECT_STORE_KIND
  value: "s3"
- name: GIT_CACHE_S3_BUCKET
  value: {{ required "config.objectStore.s3.bucket is required when objectStore.kind is s3" .Values.config.objectStore.s3.bucket | quote }}
- name: GIT_CACHE_S3_PREFIX
  value: {{ .Values.config.objectStore.s3.prefix | quote }}
{{- if .Values.config.objectStore.s3.endpoint }}
- name: GIT_CACHE_S3_ENDPOINT
  value: {{ .Values.config.objectStore.s3.endpoint | quote }}
{{- end }}
{{- else }}
- name: GIT_CACHE_OBJECT_STORE_KIND
  value: "local"
- name: GIT_CACHE_OBJECT_STORE_ROOT
  value: {{ .Values.config.objectStore.local.root | quote }}
{{- end }}
{{- if .Values.aws.region }}
- name: AWS_REGION
  value: {{ .Values.aws.region | quote }}
{{- end }}
{{- if .Values.aws.existingSecret }}
- name: AWS_ACCESS_KEY_ID
  valueFrom:
    secretKeyRef:
      name: {{ .Values.aws.existingSecret }}
      key: AWS_ACCESS_KEY_ID
- name: AWS_SECRET_ACCESS_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.aws.existingSecret }}
      key: AWS_SECRET_ACCESS_KEY
{{- end }}
{{- if .Values.upstreamAuth.existingSecret }}
- name: GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV
  value: {{ .Values.upstreamAuth.tokenEnv | quote }}
- name: {{ .Values.upstreamAuth.tokenEnv }}
  valueFrom:
    secretKeyRef:
      name: {{ .Values.upstreamAuth.existingSecret }}
      key: {{ .Values.upstreamAuth.secretKey }}
{{- end }}
{{- with .Values.config.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end }}
{{- end }}
