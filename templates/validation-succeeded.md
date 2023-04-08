## Configuration validation succeeded

#### ✅ The proposed configuration changes are valid!

## Configuration changes

{% let (directory_changes, directory_base_ref_config_status) = directory_changes %}
{%- if !directory_changes.is_empty() || directory_base_ref_config_status.is_invalid() %}

### Directory

{%- if directory_base_ref_config_status.is_invalid() %}
The configuration in the base reference is not valid, so I cannot check what has changed. Please review changes manually.
{% endif %}
{% for change in directory_changes %}
{{- change.template_format().unwrap() }}
{% endfor %}
{% endif %}

{%- for (service_name, (service_changes, base_ref_config_status)) in services_changes -%}
{%- if !service_changes.is_empty() || base_ref_config_status.is_invalid() %}
### {{ service_name }}
{%- if base_ref_config_status.is_invalid() %}
The configuration in the base reference is not valid, so I cannot check what has changed. Please review changes manually.
{% endif %}
{%- if !service_changes.is_empty() %}
{% for change in service_changes %}
{{ change }}
{%- endfor %}
{% endif %}
{% endif %}
{%- endfor %}

***

🔸 **Please review the changes detected as they will be applied *immediately* once this PR is merged** 🔸
