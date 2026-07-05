# 00 — Rough Idea (verbatim)

look I want to build a tool that would structure how I am working with agents. The tool is centured around Projects and tasks. Each task uses one of the several predefined workflows. Here are those that I have in mind right now.

Type 1. Simple chat with an agent. It might be something quick that I need to do, it might be an investigation of a bug that would require an agent to get the logs using scills and work with me back and forth on the theoris and so on

Type 2. Working on design doc. I need to write a design doc, I am telling the agent my idea agent is doing research we are writing a doc together I can comment on the doc it improves it.

Type 3. Coding task implementation. I start the process by givining an agent a well defined task. It works in a loop. first it produces the code then we have another reviewer agent that tests the code and see if all requirements are met, if not it asks coding agent to implement a change. Once internal reviewer is happy we make a PR and poll the status until the external reviewers approve if they make a comments asking for improvments we fix them. Or if linters or tests failed we fix them so we work in the loop until the code is ready and approved


Now regarding the interface. I see we might need to interfaces, CLI interface and UI interface. UI interface should allow us to have visibility on all the tasks that are being invoked it should allow us to start a new task based on the workflow. UI is basically for humans, CLI should be there for agents to be able to interact with the tool, start a task themselves and get it status

The tasks itself should allow customer to customize them by picking a an agent / a model. I also want to have a way to customize how it calls AI. By default it can call "claude exec" API  but I want an abstraction around it, so we can later plugin codex or gemini cli or anything else.

Regardin lanuage. I think the tool and the backend part should be written on Rust. I want it to store the state in SQL Lite or any other DB so if it is terminated we can restart. The UI can be written on TypeScript React for example
