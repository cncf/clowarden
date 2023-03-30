## Configuration validation succeeded

#### ✅ The proposed configuration changes are valid!

## Configuration changes

{%- if directory_changes.is_some() %}

### Directory

{% for change in directory_changes.as_ref().unwrap() %}
{{- change|format_directory_change }}
{% endfor %}
{% endif %}

{%- for (plugin_name, plugin_changes) in plugins_changes -%}
### {{ plugin_name }} plugin
{% for change in plugin_changes %}
{{ change }}
{%- endfor %}
{%- endfor %}

***

🔸 **Please review the execution plans as they will be applied *immediately* once this PR is merged** 🔸
