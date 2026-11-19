{{- define "chappe-bus.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "chappe-bus.fullname" -}}
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

{{- define "chappe-bus.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "chappe-bus.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "chappe-bus.selectorLabels" -}}
app.kubernetes.io/name: {{ include "chappe-bus.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/* The Secret with the tokens: the given one, or the one the chart makes. */}}
{{- define "chappe-bus.tokensSecret" -}}
{{- if .Values.tokens.existingSecret }}{{ .Values.tokens.existingSecret }}{{ else }}{{ include "chappe-bus.fullname" . }}-tokens{{ end }}
{{- end }}

{{/* The Secret with the webhook secret. */}}
{{- define "chappe-bus.webhookSecret" -}}
{{- if .Values.webhook.existingSecret }}{{ .Values.webhook.existingSecret }}{{ else }}{{ include "chappe-bus.fullname" . }}-webhook{{ end }}
{{- end }}

{{- define "chappe-bus.claimName" -}}
{{- if .Values.persistence.existingClaim }}{{ .Values.persistence.existingClaim }}{{ else }}{{ include "chappe-bus.fullname" . }}-data{{ end }}
{{- end }}
