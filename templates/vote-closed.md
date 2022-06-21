## Vote closed

The vote {% if results.passed %}**passed**! 🎉{% else %}**did not pass**.{% endif %}

`{{ "{:.2}"|format(results.in_favor_percentage) }}%` of the users with binding vote were in favor (passing threshold: `{{ results.pass_threshold }}%`).

### Summary

|        In favor        |        Against        |       Abstain        |        Not voted        |
| :--------------------: | :-------------------: | :------------------: | :---------------------: |
| {{ results.in_favor }} | {{ results.against }} | {{ results.abstain}} | {{ results.not_voted }} |

{% if !results.votes.is_empty() %}
### Binding votes

| User | Vote  |
| ---- | :---: |
{% for (user, vote) in results.votes -%}
| @{{ user }} | {{ vote }} |
{% endfor %}
{% endif %}
