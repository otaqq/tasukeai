{{- define "tasukeai.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "tasukeai.fullname" -}}
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

{{- define "tasukeai.selectorLabels" -}}
app.kubernetes.io/name: {{ include "tasukeai.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "tasukeai.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "tasukeai.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "tasukeai.configMapName" -}}
{{- if .Values.existingConfigMap }}
{{- .Values.existingConfigMap }}
{{- else }}
{{- include "tasukeai.fullname" . }}
{{- end }}
{{- end }}
