```markdown
# jcode Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill introduces the core development conventions and workflows for the `jcode` repository, a Rust codebase. It covers file organization, code style, commit message standards, and testing patterns to ensure consistency and maintainability. Whether you're contributing new features or fixing bugs, following these patterns will help you align with the project's best practices.

## Coding Conventions

### File Naming
- Use **PascalCase** for file names.
  - **Example:**  
    `MyModule.rs`  
    `UserProfile.rs`

### Import Style
- Use **relative imports** within the codebase.
  - **Example:**
    ```rust
    mod utils;
    use crate::utils::Helper;
    ```

### Export Style
- Use **named exports** to expose functions, structs, or modules.
  - **Example:**
    ```rust
    pub struct MyStruct { /* ... */ }
    pub fn do_something() { /* ... */ }
    ```

### Commit Messages
- Follow the **conventional commit** format.
- Use the prefix `fix` for bug fixes.
- Keep commit messages concise (average ~57 characters).
  - **Example:**
    ```
    fix: correct off-by-one error in index calculation
    ```

## Workflows

### Bug Fixing
**Trigger:** When a bug is identified and needs to be resolved  
**Command:** `/fix-bug`

1. Create a new branch for your fix.
2. Locate the relevant module (use PascalCase file names).
3. Make the necessary code changes.
4. Write or update tests in corresponding `*.test.*` files.
5. Commit your changes using the `fix:` prefix.
6. Open a pull request for review.

### Adding a New Module
**Trigger:** When introducing new functionality  
**Command:** `/add-module`

1. Create a new file using PascalCase (e.g., `NewFeature.rs`).
2. Implement the module with relative imports as needed.
3. Export structs/functions using named exports.
4. Add or update tests in a `NewFeature.test.rs` file.
5. Commit with a descriptive message (e.g., `feat: add NewFeature module`).
6. Submit a pull request.

## Testing Patterns

- **Test files** follow the pattern `*.test.*` (e.g., `UserProfile.test.rs`).
- Place tests alongside or near the modules they cover.
- Testing framework is unspecified; use Rust's built-in testing features unless otherwise noted.
  - **Example:**
    ```rust
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_feature() {
            assert_eq!(do_something(), expected_value);
        }
    }
    ```

## Commands
| Command      | Purpose                                 |
|--------------|-----------------------------------------|
| /fix-bug     | Start the bug fixing workflow           |
| /add-module  | Begin adding a new module               |
```
