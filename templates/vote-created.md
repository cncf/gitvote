## Vote created

**@{{ creator }}** has called for a vote on `{{ issue_title }}` (#{{ issue_number }}).

{% if !teams.is_empty() || !users.is_empty() %}
  {% if !teams.is_empty() ~%}
    The members of the following teams have binding votes:

    {{~ "| Team |" }}
    {{~ "| ---- |" }}
    {%- for team in teams ~%}
      | @{{ org }}/{{ team }} {{ "|" -}}
    {% endfor %}
  {% endif -%}

  {% if !users.is_empty() ~%}
    The following users have binding votes:

    {{~ "| User |" }}
    {{~ "| ---- |" }}
    {%- for user in users ~%}
      | @{{ user }} {{ "|" -}}
    {% endfor %}
  {% endif -%}

{% else ~%}
  All repository collaborators have binding votes.
{% endif %}

Non-binding votes are also appreciated as a sign of support!

## How to vote

You can cast your vote by reacting to `this` comment. The following reactions are supported:

| In favor | Against | Abstain |
| :------: | :-----: | :-----: |
|    👍     |    👎    |    👀    |

*Please note that voting for multiple options is not allowed and those votes won't be counted.*

The vote will be open for `{{ duration }}`. It will pass if at least `{{ pass_threshold }}%` of the users with binding votes vote `In favor 👍`. Once it's closed, results will be published here as a new comment.
