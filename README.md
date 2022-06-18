# GitVote

**GitVote** is a GitHub application that allows holding a vote on *issues* and *pull requests*.

## Usage

The first step is to install the **GitVote** application in the organization or repositories you'd like.

Once the application has been installed we can proceed with its configuration.

### Configuration

GitVote expects a configuration file named [.gitvote.yml](https://github.com/cncf/gitvote/blob/main/docs/config/.gitvote.yml) at the root of each repository where you'd like to create votes. Please note that the configuration file is **required** and no commands will be processed if it cannot be found. Once a vote is created, the configuration it will use during its lifetime will be the one present at the vote creation moment.

For more information about the configuration file format please see the [reference documentation](https://github.com/cncf/gitvote/blob/main/docs/config/.gitvote.yml).

### Creating votes

Votes can be created by adding a comment to an existent *issue* or *pull request* with the `/vote` command. Alternatively, if you have setup multiple configuration profiles, you can start votes using any of them with the command `/vote-PROFILE`.

![create-vote](docs/screenshots/create-vote.png)

Only repositories collaborators can create votes. For organization-owned repositories, the list of collaborators includes outside collaborators, organization members that are direct collaborators, organization members with access through team memberships, organization members with access through default organization permissions, and organization owners.

Shortly after the comment with the `/vote` command is posted, the vote will be created and the bot will post a new comment to the corresponding issue or pull request with the vote instructions.

![create-vote](docs/screenshots/vote-created.png)

*Please note that GitVote only detects commands when a comment is created, not when it's edited.*

### Voting

Users can cast their votes by reacting to the `git-vote` bot comment where the vote was created (screenshot above).

It is possible to vote `in favor`, `against` or to `abstain`, and each of these options can be selected with the following reactions:

| In favor | Against | Abstain |
| :------: | :-----: | :-----: |
|    👍     |    👎    |    👀    |

Only votes from users with a binding vote as defined in the configuration file will be counted.

*Please note that voting multiple options is not allowed and those votes won't be counted.*

### Closing votes

Once the vote time is up, the vote will be automatically closed and the results will be published in a new comment.

![create-vote](docs/screenshots/vote-closed.png)

## Contributing

Please see [CONTRIBUTING.md](./CONTRIBUTING.md) for more details.

## Code of Conduct

This project follows the [CNCF Code of Conduct](https://github.com/cncf/foundation/blob/master/code-of-conduct.md).

## License

GitVote is an Open Source project licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0).
