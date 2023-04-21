## Reconciliation completed
{% if !errors_found ~%}
  #### ✅ The reconciliation completed successfully and all changes have been applied across the services!
  {{~ "" +}}
  {{~ "## Changes applied" -}}

  {% for (service_name, changes) in changes_applied %}
    {% if !changes.is_empty() ~%}
      ### {{ service_name|capitalize }}

      {%~ for change_applied in changes %}
        {{~ change_applied.change.template_format().unwrap() -}}
      {% endfor %}
    {% endif %}
  {%- endfor %}

{% else ~%}
  #### ‼️ Some errors were found during the reconciliation
  {% for service_name in services ~%}
    ## {{ service_name|capitalize -}}

    {% if changes_applied.get(service_name.to_owned()).is_some() %}
      {% for change_applied in changes_applied.get(service_name.to_owned()).unwrap() %}
        {%- if change_applied.error.is_some() ~%}
          - Error applying change: **{{ "{:?}"|format(change_applied.change) }}**

          {{~ "```" +}}
          {{~ change_applied.error.as_ref().unwrap() }}
          {{~ "```" +}}
        {% endif -%}
      {% endfor %}
    {% endif %}

    {% if errors.get(service_name.to_owned()).is_some() -%}
      {{~ "```" +}}
      {{~ errors.get(service_name.to_owned()).as_ref().unwrap() }}
      {{~ "```" +}}
    {%- endif %}
  {%- endfor %}
{% endif -%}
