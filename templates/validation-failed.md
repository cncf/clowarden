## Configuration validation failed

#### ‼️ Some errors were found validating the configuration files

{% for err in errors %}
```
{{ "{:?}"|format(err) }}
```
{% endfor %}

***

For more details about the configuration files format please see the [documentation](https://github.com/cncf/clowarden).

🔺 **These errors must be addressed before this PR can be merged** 🔺
