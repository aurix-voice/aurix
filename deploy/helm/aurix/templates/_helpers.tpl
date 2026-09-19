{{/*
Expand the name of the chart.
*/}}
{{- define "aurix.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Fully qualified app name (release-name aware).
*/}}
{{- define "aurix.fullname" -}}
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

{{- define "aurix.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "aurix.labels" -}}
helm.sh/chart: {{ include "aurix.chart" . }}
{{ include "aurix.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: aurix
aurix.io/region: {{ .Values.config.region | quote }}
{{- end }}

{{- define "aurix.selectorLabels" -}}
app.kubernetes.io/name: {{ include "aurix.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "aurix.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "aurix.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "aurix.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}

{{- define "aurix.secretName" -}}
{{- default (include "aurix.fullname" .) .Values.existingSecret }}
{{- end }}

{{- define "aurix.headlessServiceName" -}}
{{- printf "%s-headless" (include "aurix.fullname" .) }}
{{- end }}

{{/*
Hostname of one pod when perNode is enabled: <pod>.<domain>.
*/}}
{{- define "aurix.perNodeHost" -}}
{{- printf "%s-%d.%s" (include "aurix.fullname" .root) (int .ordinal) .root.Values.perNode.domain }}
{{- end }}

{{/*
Non-secret environment shared by the server pods and the migration job.
*/}}
{{- define "aurix.env" -}}
{{- /* Context: dict "root" $ "job" bool. The migration job only touches the database, so it
       runs with migrations on, TURN off and a documentation-range media IP that merely has
       to pass validation. */ -}}
- name: POD_NAME
  valueFrom:
    fieldRef:
      fieldPath: metadata.name
- name: AURIX__SERVER__ENVIRONMENT
  value: production
- name: AURIX__SERVER__API_PORT
  value: {{ .root.Values.ports.api | quote }}
- name: AURIX__SERVER__WS_PORT
  value: {{ .root.Values.ports.ws | quote }}
- name: AURIX__SERVER__REGION
  value: {{ .root.Values.config.region | quote }}
{{- if and .root.Values.perNode.enabled .root.Values.perNode.setExternalUrls }}
- name: AURIX__SERVER__EXTERNAL_URL
  value: {{ printf "https://$(POD_NAME).%s" .root.Values.perNode.domain | quote }}
- name: AURIX__SERVER__EXTERNAL_WS_URL
  value: {{ printf "wss://$(POD_NAME).%s/ws" .root.Values.perNode.domain | quote }}
{{- else }}
- name: AURIX__SERVER__EXTERNAL_URL
  value: {{ .root.Values.config.externalUrl | quote }}
{{- with .root.Values.config.externalWsUrl }}
- name: AURIX__SERVER__EXTERNAL_WS_URL
  value: {{ . | quote }}
{{- end }}
{{- end }}
- name: AURIX__SERVER__CORS_ORIGINS
  value: {{ join "," .root.Values.config.corsOrigins | quote }}
- name: AURIX__SERVER__TRUSTED_PROXIES
  value: {{ join "," .root.Values.config.trustedProxies | quote }}
{{- if and .root.Values.config.location.latitude .root.Values.config.location.longitude }}
- name: AURIX__SERVER__LOCATION__LATITUDE
  value: {{ .root.Values.config.location.latitude | quote }}
- name: AURIX__SERVER__LOCATION__LONGITUDE
  value: {{ .root.Values.config.location.longitude | quote }}
{{- end }}
- name: AURIX__DATABASE__RUN_MIGRATIONS
  value: {{ or .job (not .root.Values.migrations.enabled) | quote }}
- name: AURIX__MEDIA__PORT
  value: {{ .root.Values.ports.media | quote }}
- name: AURIX__TURN__ENABLED
  value: {{ and (not .job) .root.Values.config.turn.enabled | quote }}
{{- if .job }}
- name: AURIX__MEDIA__EXTERNAL_IP
  value: "203.0.113.1"
{{- end }}
- name: AURIX__TURN__UDP_PORT
  value: {{ .root.Values.ports.turn | quote }}
- name: AURIX__TURN__TCP_PORT
  value: {{ .root.Values.ports.turn | quote }}
- name: AURIX__TURN__REALM
  value: {{ default "aurix" .root.Values.config.turn.realm | quote }}
- name: AURIX__TURN__MIN_PORT
  value: {{ .root.Values.config.turn.minPort | quote }}
- name: AURIX__TURN__MAX_PORT
  value: {{ .root.Values.config.turn.maxPort | quote }}
- name: AURIX__RECORDING__ENABLED
  value: {{ .root.Values.config.recording.enabled | quote }}
- name: AURIX__RECORDING__ENCRYPTION_ENABLED
  value: {{ .root.Values.config.recording.encryptionEnabled | quote }}
{{- with .root.Values.config.recording.s3 }}
{{- if .bucket }}
- name: AURIX__RECORDING__S3_BUCKET
  value: {{ .bucket | quote }}
- name: AURIX__RECORDING__S3_REGION
  value: {{ .region | quote }}
{{- with .endpoint }}
- name: AURIX__RECORDING__S3_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- end }}
{{- end }}
- name: AURIX__METRICS__PORT
  value: {{ .root.Values.ports.metrics | quote }}
- name: AURIX__TRACING__LOG_LEVEL
  value: {{ .root.Values.config.logLevel | quote }}
- name: AURIX__TRACING__LOG_FORMAT
  value: json
{{- range $k, $v := .root.Values.config.extraEnv }}
- name: {{ $k }}
  value: {{ $v | quote }}
{{- end }}
{{- end }}
