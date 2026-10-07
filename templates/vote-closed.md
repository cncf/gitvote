{%- block introduction %}{%+ endblock -%}
## {% block title %}Vote closed{% endblock %}

The vote {% if results.passed %}**passed**! 🎉{% else %}**did not pass**.{% endif %}

In favor: `{{ "{:.2}"|format(results.in_favor_percentage) }}%` · Against: `{{ "{:.2}"|format(results.against_percentage) }}%`{% if let Some((minimum_approvals, maximum_rejections)) = results.vote_count_thresholds() %} · Required: at least `{{ minimum_approvals }}` in favor and at most `{{ maximum_rejections }}` against{% else if let Some(pass_threshold) = results.percentage_threshold() %} · Passing threshold: `{{ pass_threshold }}%`{% endif %}

### Summary

|        In favor        |        Against        |       Abstain        |        Not voted        |
| :--------------------: | :-------------------: | :------------------: | :---------------------: |
| {{ results.in_favor }} | {{ results.against }} | {{ results.abstain}} | {{ results.not_voted }} |

{%~ if !results.votes.is_empty() -%}
  {%- if results.binding > 0 ~%}
    ### Binding votes ({{ results.binding }})
    {{ "" }}
    {{~ "| User | Vote  | Timestamp |" }}
    {{~ "| ---- | :---: | :-------: |" }}
    {%- for (user, vote) in results.votes ~%}
      {%- if vote.binding ~%}
        | @{{ user }} | {{ vote.vote_option }} | {{ vote.timestamp }} {{ "|" -}}
      {% endif -%}
    {% endfor %}
  {% endif -%}

  {% if results.non_binding > 0 ~%}
    <details>
      <summary><h3>Non-binding votes ({{ results.non_binding }})</h3></summary>

      {%~ let max_non_binding = 300 %}
      {%- if results.non_binding > max_non_binding %}
        <i>(displaying only the first {{ max_non_binding }} non-binding votes)</i>
      {%- endif %}

      {{~ "| User | Vote  | Timestamp |" }}
      {{~ "| ---- | :---: | :-------: |" }}
      {%- for (user, vote) in results.votes|non_binding(max_non_binding) ~%}
        | @{{ user }} | {{ vote.vote_option }} | {{ vote.timestamp }} {{ "|" -}}
      {% endfor ~%}
    </details>
  {% endif -%}
{% endif -%}
