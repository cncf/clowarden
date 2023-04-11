## Configuration validation succeeded

#### ✅ The proposed configuration changes are valid!

## Configuration changes

{%- if !directory_changes.changes.is_empty() || directory_changes.base_ref_config_status.is_invalid() %}

### Directory

{%- if directory_changes.base_ref_config_status.is_invalid() %}
The configuration in the base reference is not valid, so I cannot check what has changed. Please review changes manually.
{% endif %}
{% for change in directory_changes.changes %}
{{- change.template_format().unwrap() }}
{% endfor %}
{% endif %}

{%- for (service_name, service_changes) in services_changes -%}
{%- if !service_changes.changes.is_empty() || service_changes.base_ref_config_status.is_invalid() %}
### {{ service_name }}
{%- if service_changes.base_ref_config_status.is_invalid() %}
The configuration in the base reference is not valid, so I cannot check what has changed. Please review changes manually.
{% endif %}
{%- if !service_changes.changes.is_empty() %}
{% for change in service_changes.changes %}
{{ change.template_format().unwrap() }}
{%- endfor %}
{% endif %}
{% endif %}
{%- endfor %}

***

🔸 **Please review the changes detected as they will be applied *immediately* once this PR is merged** 🔸
