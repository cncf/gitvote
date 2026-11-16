{{/* vim: set filetype=mustache: */}}
{{/*
Expand the name of the chart.
*/}}
{{- define "chart.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "chart.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "chart.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels
*/}}
{{- define "chart.labels" -}}
helm.sh/chart: {{ include "chart.chart" . }}
{{ include "chart.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
Selector labels
*/}}
{{- define "chart.selectorLabels" -}}
app.kubernetes.io/name: {{ include "chart.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Return the GitHub App ID in decimal, failing on any value that is not a valid App ID.
Unquoted numbers in a values file are read as floats, so whole floats are accepted and
printed in decimal instead of in scientific notation. They must be below 2^53, as floats
cannot hold every whole number past it, so the value read may differ from the one
written: larger IDs must be quoted. Strings are accepted only in decimal without leading
zeros, as helm would read 0123 as 83, and within the int64 range the service uses. An
unquoted 0123 in a values file reaches the chart as 83 already, so it cannot be refused.
Helm versions built before Go 1.18 evaluate every argument of and, so the float checks
are nested to run on floats only.
*/}}
{{- define "chart.githubAppID" -}}
{{- $appID := .Values.gitvote.github.appID -}}
{{- $text := "" -}}
{{- if kindIs "int64" $appID -}}
{{- $text = toString $appID -}}
{{- else if kindIs "float64" $appID -}}
{{- if and (lt $appID 9007199254740992.0) (eq (float64 (int64 $appID)) $appID) -}}
{{- $text = toString (int64 $appID) -}}
{{- end -}}
{{- else if kindIs "string" $appID -}}
{{- $text = $appID -}}
{{- end -}}
{{- if not (and (regexMatch "^[1-9][0-9]*$" $text) (eq (toString (int64 $text)) $text)) -}}
{{- fail "gitvote.github.appID must be the ID of the GitHub App, a positive whole number written without leading zeros" -}}
{{- end -}}
{{- $text -}}
{{- end -}}

{{/*
Kubernetes version
Built-in object .Capabilities.KubeVersion.Minor can provide non-number output
For example on GKE it returns "15+" instead of "15"
*/}}
{{- define "chart.KubernetesVersion" -}}
{{- $minorVersion := .Capabilities.KubeVersion.Minor | regexFind "[0-9]+" -}}
{{- printf "%s.%s" .Capabilities.KubeVersion.Major $minorVersion -}}
{{- end -}}

{{/*
Kubernetes resource name prefix
Using the prefix allows deploying multiple instances of the Chart in a single Kubernetes namespace.
If the dynamic resource name prefix is disabled, this template results in an empty string.
As described in `chart.fullname`, the length limit for some resource names is 63.
Therefore, we truncate the prefix to have a max-length of 44 to respect this limit considering the
longest resource name ("dbmigrator-install" = 18 chars).
*/}}
{{- define "chart.resourceNamePrefix" -}}
{{- if .Values.dynamicResourceNamePrefixEnabled -}}
{{- (include "chart.fullname" .) | trunc 43 | trimSuffix "-" | printf "%s-" -}}
{{- end -}}
{{- end -}}

{{/*
Fail on values that must be strings but are not, as helm would print a different value.
Unquoted numbers in a values file are read as floats and printed in scientific notation
from 7 digits, and YAML words like yes or off are read as booleans and printed as true or
false. Other floats are refused too, as the value written cannot be told from them: 0123
and 1.50 reach the chart as 83 and 1.5. Numbers from --set are integers and are printed
unchanged, but helm stores them with the release and reads them back as floats, so they
are refused on upgrades using --reuse-values until set again as strings.
*/}}
{{- define "chart.validateStrings" -}}
{{- $github := .Values.gitvote.github -}}
{{- $values := dict
  "db.dbname" .Values.db.dbname
  "db.password" .Values.db.password
  "db.user" .Values.db.user
  "gitvote.github.webhookSecret" $github.webhookSecret
  "gitvote.github.webhookSecretFallback" $github.webhookSecretFallback
-}}
{{- range $name, $value := $values -}}
{{- if not (or (kindIs "string" $value) (kindIs "int64" $value) (kindIs "invalid" $value)) -}}
{{- fail (printf "%s must be a string, helm corrupts numbers and booleans: quote it in the values file or pass it with --set-string" $name) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Provide an init container to verify the database is accessible
*/}}
{{- define "chart.checkDbIsReadyInitContainer" -}}
{{- $securityContext := default (dict) .Values.checkDbIsReadyInitContainer.securityContext }}
name: check-db-ready
{{ if .Values.postgresql.image.registry -}}
image: {{ .Values.postgresql.image.registry }}/{{ .Values.postgresql.image.repository }}:{{ .Values.postgresql.image.tag }}
{{- else }}
image: {{ .Values.postgresql.image.repository }}:{{ .Values.postgresql.image.tag }}
{{- end }}
imagePullPolicy: {{ .Values.pullPolicy }}
env:
  - name: PGHOST
    value: {{ default (printf "%s-postgresql.%s" .Release.Name .Release.Namespace) .Values.db.host | quote }}
  - name: PGPORT
    value: {{ .Values.db.port | int | quote }}
  - name: PGUSER
    value: {{ .Values.db.user | quote }}
{{- if $securityContext }}
securityContext:{{- toYaml $securityContext | nindent 2 }}
{{- else }}
securityContext: {}
{{- end }}
command: ['sh', '-c', 'until pg_isready; do echo waiting for database; sleep 2; done;']
{{- end -}}
