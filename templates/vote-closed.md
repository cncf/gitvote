## Vote closed

The vote {% if passed %}**passed**! 🎉{% else %}**did not pass**.{% endif %}

`{{ in_favor_percentage }}%` of the users with binding vote were in favor (passing threshold: `{{ pass_threshold }}%`).

### Summary

|    In favor    |    Against    |   Abstain    |    Not voted    |
| :------------: | :-----------: | :----------: | :-------------: |
| {{ in_favor }} | {{ against }} | {{ abstain}} | {{ not_voted }} |

### Binding votes

| User | Vote  |
| ---- | :---: |
{% for (user, vote) in voters -%}
| @{{ user }} | {{ vote }} |
{% endfor %}
