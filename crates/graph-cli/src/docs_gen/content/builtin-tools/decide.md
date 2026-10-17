---
summary: Asks a decision model typed questions about one piece of data.
output:
  description: 'An object with one answer per question, keyed by the question''s name in `questions`. A step that asks `urgent` and `team` returns `{"urgent": {...}, "team": {...}}`, and later steps read `{{E1.urgent.probability}}` or `{{E1.team.choice}}`.'
  type: object
  additionalProperties:
    type: object
    description: The answer to the question with this name. Which fields it has depends on the question's `type`.
    properties:
      type:
        type: string
        enum: [likelihood, choice, score]
        description: The question's type.
      probability:
        type: number
        description: Likelihood only. How likely the statement is true, from 0 to 1.
      choice:
        type: string
        description: Choice only. The key of the chosen option in `criteria`.
      score:
        type: number
        description: Score only. The probability-weighted level, from 0 (the first entry in `criteria`) to one less than the number of levels.
      confidence:
        type: number
        description: Choice and score only. How sure the model is of the answer, from 0 to 1.
      probabilities:
        type: object
        description: 'Choice and score only. Each option''s probability, keyed by option for a choice (`{"billing": 0.98, "technical": 0.02}`) and by level index for a score (`{"0": 0.0, "1": 0.01, "2": 0.99}`).'
      legend:
        type: object
        description: 'Score only. Each level index mapped to its entry in `criteria`, such as `{"0": "Calm", "1": "Annoyed", "2": "Angry"}`.'
---

## Behavior

- Each question is answered independently, in one call.
- Question types: `likelihood` (a probability), `choice` (one option) and `score` (a level on a scale).
- The result maps each question name to its answer, such as `{{E1.urgent.probability}}`.
- Needs a decision model role, `decider` by default.

## When to use it

- Put the data in `state` and keep the questions short.
