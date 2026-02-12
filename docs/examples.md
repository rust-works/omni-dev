# Real-World Examples

Comprehensive examples showing omni-dev in action across different project
types and scenarios.

## Table of Contents

1. [Basic Usage Examples](#basic-usage-examples)
2. [Web Application Project](#web-application-project)
3. [Rust CLI Project](#rust-cli-project)
4. [Node.js API Server](#nodejs-api-server)
5. [React Frontend](#react-frontend)
6. [Python Data Science](#python-data-science)
7. [Enterprise Monorepo](#enterprise-monorepo)
8. [Before/After Showcases](#beforeafter-showcases)

## Basic Usage Examples

### Single Commit Improvement

**Before**:

```
$ git log --oneline -1
a1b2c3d fix stuff
```

**Command**:

```bash
omni-dev git commit message twiddle 'HEAD^..HEAD' --use-context
```

**After**:

```
$ git log --oneline -1
a1b2c3d feat(auth): implement JWT token validation middleware
```

### Feature Branch Cleanup

**Before**:

```bash
$ git log --oneline main..HEAD
e4b2c1a more updates
d3a1f2e fix things
c7e1b4f wip auth
b9f2a6d initial work
```

**Command**:

```bash
omni-dev git commit message twiddle 'main..HEAD' --use-context
```

**After**:

```bash
$ git log --oneline main..HEAD
e4b2c1a docs(auth): add JWT authentication guide
d3a1f2e fix(auth): resolve token expiration edge case
c7e1b4f feat(auth): add OAuth2 Google integration
b9f2a6d feat(auth): implement JWT token middleware
```

## Web Application Project

### Project Structure

```
webapp/
├── .omni-dev/
│   ├── scopes.yaml
│   └── commit-guidelines.md
├── backend/
│   ├── api/
│   ├── auth/
│   └── models/
├── frontend/
│   ├── components/
│   ├── pages/
│   └── utils/
└── docs/
```

### Configuration (`.omni-dev/scopes.yaml`)

```yaml
scopes:
  - name: "auth"
    description: "Authentication and authorization"
    examples:
      - "auth: add OAuth2 Google integration"
      - "auth: fix JWT token validation"
    file_patterns:
      - "backend/auth/**"
      - "frontend/auth/**"
      - "auth.js"
      - "login.vue"

  - name: "api"
    description: "Backend API endpoints"
    examples:
      - "api: add user management endpoints"
      - "api: improve error handling"
    file_patterns:
      - "backend/api/**"
      - "routes/**"
      - "controllers/**"

  - name: "ui"
    description: "Frontend user interface"
    examples:
      - "ui: add responsive navigation"
      - "ui: fix mobile layout"
    file_patterns:
      - "frontend/components/**"
      - "frontend/pages/**"
      - "*.vue"
      - "*.jsx"
      - "styles/**"

  - name: "db"
    description: "Database and models"
    examples:
      - "db: add user profiles table"
      - "db: optimize query performance"
    file_patterns:
      - "backend/models/**"
      - "migrations/**"
      - "*.sql"
```

### Example Workflow

**Scenario**: Adding user profile feature

**Initial commits**:

```bash
$ git log --oneline -5
f3e4d5c update stuff
e2d3c4b add more features  
d1c2b3a fix db
c9b8a7f wip profiles
b8a7c6d start user work
```

**Command**:

```bash
omni-dev git commit message twiddle 'HEAD~5..HEAD' --use-context
```

**Improved commits**:

```bash
$ git log --oneline -5
f3e4d5c docs(api): add user profile endpoints documentation
e2d3c4b ui(profile): implement responsive profile editor form
d1c2b3a fix(db): resolve user profile cascade deletion issue
c9b8a7f feat(api): add user profile CRUD endpoints
b8a7c6d feat(db): create user profiles table with validation
```

## Rust CLI Project

### Project Structure

```
rust-cli/
├── .omni-dev/
│   ├── scopes.yaml
│   └── commit-guidelines.md
├── src/
│   ├── cli/
│   ├── core/
│   ├── utils/
│   └── lib.rs
├── tests/
└── benches/
```

### Configuration

```yaml
scopes:
  - name: "cli"
    description: "Command-line interface"
    examples:
      - "cli: add new subcommand for exports"
      - "cli: improve help text formatting"
    file_patterns:
      - "src/cli/**"
      - "src/main.rs"

  - name: "core"
    description: "Core library functionality"
    examples:
      - "core: implement async file processing"
      - "core: add error recovery mechanisms"
    file_patterns:
      - "src/core/**"
      - "src/lib.rs"
      - "src/processor/**"

  - name: "tests"
    description: "Test improvements"
    examples:
      - "tests: add integration tests for CLI"
      - "tests: improve test coverage for edge cases"
    file_patterns:
      - "tests/**"
      - "src/**/tests.rs"
      - "benches/**"
```

### Example: Adding New Feature

**Before**:

```bash
$ git log --oneline -4
h5g6f7e more changes
g4f5e6d fix tests
f3e4d5c add json export
e2d3c4b update cli
```

**Command**:

```bash
omni-dev git commit message twiddle 'HEAD~4..HEAD' --use-context
```

**After**:

```bash
$ git log --oneline -4
h5g6f7e docs(cli): add examples for JSON export functionality
g4f5e6d tests(core): fix async processing tests for edge cases
f3e4d5c feat(core): implement JSON export with schema validation
e2d3c4b feat(cli): add --export-json flag with format options
```

## Node.js API Server

### Project Structure

```
api-server/
├── .omni-dev/
├── src/
│   ├── controllers/
│   ├── middleware/
│   ├── models/
│   ├── routes/
│   └── utils/
├── tests/
└── docs/
```

### Real Scenario: Adding Rate Limiting

**Initial messy commits**:

```bash
$ git log --oneline -6
j7h8i9k stuff
i6h7j8k update middleware
h5g6f7e add rate limit
g4f5e6d fix stuff
f3e4d5c more rate limit work
e2d3c4b wip
```

**Files changed**:

```bash
$ git diff --name-only HEAD~6..HEAD
src/middleware/rateLimit.js
src/routes/api.js  
src/config/redis.js
tests/middleware/rateLimit.test.js
docs/api-rate-limiting.md
package.json
```

**Command with batching**:

```bash
omni-dev git commit message twiddle 'HEAD~6..HEAD' --use-context --concurrency 3
```

**Professional result**:

```bash
$ git log --oneline -6  
j7h8i9k docs(api): add comprehensive rate limiting documentation

Add detailed guide for API rate limiting:
- Configuration options and Redis setup
- Rate limit headers and client handling  
- Monitoring and alerting recommendations
- Performance impact analysis

i6h7j8k feat(middleware): add Redis-based distributed rate limiting

Implement production-ready rate limiting:
- Configurable limits per endpoint and user type
- Redis backend for distributed deployments
- Graceful degradation when Redis unavailable
- Custom rate limit headers for client feedback

h5g6f7e feat(routes): integrate rate limiting with API endpoints

- Apply rate limits to all public API routes
- Different limits for authenticated vs anonymous users
- Special handling for admin and service accounts
- Comprehensive error responses with retry information

g4f5e6d fix(config): resolve Redis connection configuration issues

- Fix Redis URL parsing for production environments  
- Add connection retry logic with exponential backoff
- Improve error handling for Redis connection failures
- Add health check endpoint for Redis connectivity

f3e4d5c feat(middleware): implement basic rate limiting infrastructure

- Create rate limiting middleware with configurable rules
- Add support for IP-based and user-based limiting
- Implement sliding window algorithm for accurate limiting
- Add comprehensive logging for rate limit violations

e2d3c4b chore(deps): add Redis and rate limiting dependencies

Update package.json with required dependencies:
- ioredis for Redis connectivity
- express-rate-limit for HTTP rate limiting
- node-cache for in-memory fallback caching
```

## React Frontend

### Project Structure

```
react-app/
├── .omni-dev/
├── src/
│   ├── components/
│   ├── pages/
│   ├── hooks/
│   ├── services/
│   └── utils/
├── public/
└── docs/
```

### Configuration

```yaml
scopes:
  - name: "ui"
    description: "User interface components"
    file_patterns:
      - "src/components/**"
      - "src/pages/**"
      - "*.tsx"
      - "*.jsx"

  - name: "hooks"
    description: "React hooks and state management"
    file_patterns:
      - "src/hooks/**"
      - "src/store/**"
      - "src/context/**"

  - name: "services"
    description: "API services and external integrations"
    file_patterns:
      - "src/services/**"
      - "src/api/**"
```

### Example: Dashboard Feature

**Before (typical React development commits)**:

```bash
$ git log --oneline -7
n9m8l7k fix styling
m8l7k9n update dashboard
l7k8j9i add charts
k6j7i8h fix responsive
j5i6h7g wip dashboard
i4h5g6f update components
h3g4f5e start dashboard
```

**After omni-dev processing**:

```bash
$ git log --oneline -7
n9m8l7k ui(dashboard): fix responsive layout issues on mobile devices
m8l7k9n feat(ui): add real-time data updates to dashboard widgets
l7k8j9i feat(ui): implement interactive charts with Chart.js integration
k6j7i8h fix(ui): resolve dashboard responsive breakpoints for tablets
j5i6h7g feat(hooks): add useDashboardData hook for state management
i4h5g6f refactor(ui): extract reusable dashboard widget components
h3g4f5e feat(ui): create dashboard layout with grid system
```

## Python Data Science

### Project Structure

```
ml-project/
├── .omni-dev/
├── src/
│   ├── data/
│   ├── models/
│   ├── analysis/
│   └── utils/
├── notebooks/
├── tests/
└── docs/
```

### Configuration

```yaml
scopes:
  - name: "data"
    description: "Data processing and ETL"
    file_patterns:
      - "src/data/**"
      - "data_pipeline/**"
      - "**/preprocessing.py"

  - name: "models"
    description: "Machine learning models"  
    file_patterns:
      - "src/models/**"
      - "models/**"
      - "**/training.py"
      - "**/inference.py"

  - name: "analysis"
    description: "Data analysis and visualization"
    file_patterns:
      - "src/analysis/**"
      - "notebooks/**"
      - "**/visualization.py"

  - name: "pipeline"
    description: "ML pipeline and orchestration"
    file_patterns:
      - "src/pipeline/**"
      - "**/workflow.py"
      - "**/orchestration.py"
```

### Example: Model Improvement

**Scientists' typical commits**:

```bash
$ git log --oneline -5
r8q7p6o try different params
q7p6o9n fix model training
p6o5n8m update notebook
o5n4m7l add feature engineering
n4m3l6k experiment with RF
```

**After contextual improvement**:

```bash
$ git log --oneline -5
r8q7p6o models(rf): optimize hyperparameters with grid search

Improve Random Forest model performance:
- Add automated hyperparameter tuning with GridSearchCV
- Increase cross-validation folds from 3 to 5
- Add feature importance analysis and logging
- Achieve 0.15 improvement in F1-score on validation set

q7p6o9n fix(models): resolve memory leak in batch training pipeline

- Fix gradient accumulation clearing in training loop
- Add proper tensor cleanup after each batch
- Reduce peak memory usage by 40% during training
- Enable training on larger datasets without OOM errors

p6o5n8m analysis(viz): add comprehensive model performance visualization

Create interactive dashboard for model analysis:
- ROC curves and precision-recall plots
- Feature importance heatmaps
- Confusion matrix with class-wise metrics
- Training history and convergence plots

o5n4m7l feat(data): implement advanced feature engineering pipeline

Add sophisticated feature transformations:
- Polynomial features with degree selection
- Time-based feature extraction (rolling stats, lags)
- Categorical encoding with target statistics
- Missing value imputation with iterative approach

n4m3l6k feat(models): add Random Forest classifier with ensemble

Implement production-ready Random Forest model:
- Configurable ensemble parameters
- Built-in feature selection with recursive elimination
- Cross-validation with stratified sampling
- Model serialization for deployment pipeline
```

## Enterprise Monorepo

### Project Structure

```
enterprise-app/
├── .omni-dev/
├── services/
│   ├── user-service/
│   ├── order-service/
│   ├── payment-service/
│   └── notification-service/
├── shared/
│   ├── common/
│   ├── types/
│   └── utils/
├── frontend/
│   ├── admin-portal/
│   ├── customer-app/
│   └── mobile-app/
└── infrastructure/
```

### Configuration

```yaml
scopes:
  # Services
  - name: "user-service"
    description: "User management microservice"
    file_patterns: ["services/user-service/**"]

  - name: "order-service"
    description: "Order processing microservice"
    file_patterns: ["services/order-service/**"]

  - name: "payment-service"
    description: "Payment processing microservice"
    file_patterns: ["services/payment-service/**"]

  # Frontend Apps
  - name: "admin-portal"
    description: "Administrative web interface"
    file_patterns: ["frontend/admin-portal/**"]

  - name: "customer-app"
    description: "Customer-facing web application"
    file_patterns: ["frontend/customer-app/**"]

  # Shared Infrastructure
  - name: "shared"
    description: "Shared libraries and utilities"
    file_patterns: ["shared/**"]

  - name: "infra"
    description: "Infrastructure and deployment"
    file_patterns: ["infrastructure/**", "*.yaml", "Dockerfile*"]
```

### Example: Cross-Service Feature

**Scenario**: Implementing order notifications across multiple services

**Before (typical enterprise commits)**:

```bash
$ git log --oneline -8
w8v7u6t update configs  
v7u6t5s fix deployment
u6t5s4r add notifications
t5s4r3q update user service
s4r3q2p wip orders
r3q2p1o fix frontend
q2p1o9n update shared
p1o9n8m start notifications
```

**Files span multiple services**:

```bash
$ git diff --name-only HEAD~8..HEAD
services/user-service/src/notifications.js
services/order-service/src/events.js  
services/notification-service/src/handlers.js
shared/types/notifications.ts
shared/events/order-events.ts
frontend/customer-app/src/notifications/
infrastructure/k8s/notification-service.yaml
docs/architecture/event-driven-notifications.md
```

**After enterprise-grade improvement**:

```bash
$ git log --oneline -8
w8v7u6t infra(k8s): configure notification service deployment

Add production Kubernetes configuration:
- Deployment with resource limits and health checks
- Service discovery and load balancer setup  
- ConfigMap for environment-specific settings
- Horizontal Pod Autoscaler for traffic scaling

v7u6t5s fix(infra): resolve notification service deployment issues

- Fix service account permissions for event access
- Update ingress configuration for proper routing
- Add missing environment variables for Redis connection
- Resolve container registry authentication problems

u6t5s4r feat(notification-service): implement order notification handlers

Add comprehensive notification processing:
- Email notifications for order status changes
- SMS notifications for critical order updates  
- Push notifications for mobile app users
- Webhook notifications for third-party integrations

t5s4r3q feat(user-service): add notification preferences management

Implement user notification controls:
- User preference storage and validation
- API endpoints for preference management
- Integration with notification service routing
- Privacy compliance for notification data

s4r3q2p feat(order-service): emit structured events for notifications

Add event-driven notification triggers:
- Structured event payloads with order context
- Reliable message publishing with retry logic
- Event schema validation and versioning
- Dead letter queue handling for failed notifications

r3q2p1o feat(customer-app): add real-time notification display

Implement customer notification interface:
- Real-time notification updates via WebSocket
- Notification history with pagination
- Mark as read/unread functionality
- Responsive design for mobile and desktop

q2p1o9n feat(shared): add notification event types and schemas

Create shared notification infrastructure:
- TypeScript interfaces for all notification types
- Event schema definitions with validation
- Utility functions for event creation and parsing
- Documentation for cross-service event contracts

p1o9n8m feat(architecture): design event-driven notification system

Implement comprehensive notification architecture:
- Event sourcing pattern for order state changes
- Decoupled notification service with message queues
- Multi-channel notification delivery (email/SMS/push)
- User preference management and delivery optimization
```

## Before/After Showcases

### Showcase 1: Open Source Project Cleanup

**Context**: Preparing for v1.0 release

**Before (contributor commits)**:

```
4a3b2c1 stuff
5b4c3d2 fix  
6c5d4e3 update readme
7d6e5f4 add feature
8e7f6g5 wip tests
9f8g7h6 more work
0g9h8i7 fix bug
1h0i9j8 initial
```

**After (professional release)**:

```
4a3b2c1 docs(examples): add comprehensive usage examples for v1.0

Add detailed code examples covering all major use cases:
- Basic configuration and setup procedures
- Advanced customization with hooks and plugins
- Integration examples for popular frameworks
- Migration guide from v0.x to v1.0

5b4c3d2 fix(core): resolve thread safety issue in concurrent operations

- Add proper synchronization for shared state access
- Implement lock-free algorithms where possible
- Add comprehensive concurrency tests
- Fix race condition in plugin initialization

6c5d4e3 docs(readme): prepare comprehensive v1.0 documentation

Major documentation overhaul for stable release:
- Complete API reference with examples
- Installation and quick start guide
- Architecture overview and design decisions
- Contributing guidelines and development setup

7d6e5f4 feat(plugins): add extensible plugin system with hooks

Implement comprehensive plugin architecture:
- Event-driven hook system for extensibility
- Plugin discovery and lifecycle management
- Type-safe plugin API with validation
- Hot-reloading support for development

8e7f6g5 test(integration): add comprehensive test suite for v1.0

Achieve 95%+ test coverage with quality test suite:
- Unit tests for all core functionality
- Integration tests for real-world scenarios  
- Performance benchmarks and regression tests
- Cross-platform compatibility testing

9f8g7h6 feat(core): implement advanced caching with invalidation

Add sophisticated caching layer:
- Multi-level cache hierarchy (memory, disk, remote)
- Intelligent cache invalidation strategies
- Cache statistics and monitoring
- Configurable cache policies per use case

0g9h8i7 fix(performance): optimize memory usage and CPU efficiency

Major performance improvements:
- Reduce memory allocations by 60% through object pooling
- Implement lazy loading for expensive operations
- Add streaming processing for large datasets
- Profile-guided optimization for hot paths

1h0i9j8 feat(core): establish foundational architecture for v1.0

Create robust foundation for stable release:
- Clean separation of concerns with modular design
- Comprehensive error handling and recovery
- Configurable logging with structured output
- Backward compatibility framework for future updates
```

### Showcase 2: Startup MVP Development

**Context**: Fast-moving startup building MVP

**Before (rapid development)**:

```
z9y8x7w deploy stuff
y8x7w6v fix prod
x7w6v5u more features
w6v5u4t fix things
v5u4t3s add auth
u4t3s2r ui updates
t3s2r1q api work
s2r1q0p mvp start
```

**After (investor-ready)**:

```
z9y8x7w deploy(prod): launch MVP v1.0 to production environment

Successfully deploy core MVP functionality:
- Automated deployment pipeline with zero-downtime updates
- Production monitoring and alerting setup
- Performance optimization for expected user load
- Security hardening and SSL certificate configuration

y8x7w6v fix(prod): resolve critical production performance bottlenecks

Emergency production fixes for launch day issues:
- Optimize database queries reducing response time by 80%
- Fix memory leaks in real-time features
- Add connection pooling for database scalability
- Implement graceful degradation for high traffic

x7w6v5u feat(ui): complete core user experience workflows

Implement essential user journeys for MVP:
- Streamlined onboarding with progressive disclosure
- Responsive design optimized for mobile-first usage
- Real-time updates and notifications
- Accessibility compliance for inclusive design

w6v5u4t fix(api): resolve data consistency and validation issues

Strengthen API reliability and data integrity:
- Add comprehensive input validation and sanitization
- Implement transaction handling for critical operations
- Fix race conditions in concurrent user scenarios
- Add proper error handling with user-friendly messages

v5u4t3s feat(auth): implement secure user authentication system

Build production-ready authentication:
- JWT-based authentication with refresh tokens
- Social login integration (Google, GitHub, LinkedIn)
- Password strength requirements and secure storage
- Account verification and password reset workflows

u4t3s2r feat(ui): build responsive user interface components

Create polished user interface for MVP launch:
- Component library with consistent design system
- Mobile-responsive layouts for all screen sizes
- Loading states and error handling for better UX
- Interactive elements with smooth animations

t3s2r1q feat(api): develop core REST API with business logic

Implement essential backend functionality:
- RESTful API design with proper HTTP semantics
- Business logic layer with validation and processing
- Database integration with optimized queries
- Comprehensive API documentation for frontend team

s2r1q0p feat(mvp): establish foundational architecture

Create scalable foundation for rapid iteration:
- Modular architecture supporting feature development
- Database schema designed for growth and flexibility
- Development workflow with testing and CI/CD
- Monitoring and logging infrastructure for production insights
```

## Project-Specific Examples

### Configuration Examples by Technology

**React/TypeScript SPA**:

```yaml
scopes:
  - name: "components"
    file_patterns: ["src/components/**/*.tsx"]
  - name: "hooks" 
    file_patterns: ["src/hooks/**/*.ts"]
  - name: "pages"
    file_patterns: ["src/pages/**/*.tsx"]
```

**Django REST API**:

```yaml
scopes:
  - name: "api"
    file_patterns: ["*/api/**", "*/serializers.py", "*/views.py"]
  - name: "models"
    file_patterns: ["*/models.py", "migrations/**"]
  - name: "auth"
    file_patterns: ["*/authentication.py", "*/permissions.py"]
```

**Rust Web Service**:

```yaml
scopes:
  - name: "handlers"
    file_patterns: ["src/handlers/**", "src/routes/**"]
  - name: "models"
    file_patterns: ["src/models/**", "src/schema.rs"]
  - name: "middleware"
    file_patterns: ["src/middleware/**"]
```

These examples show omni-dev's versatility across different:

- **Project types**: Web apps, CLI tools, APIs, data science, enterprise
- **Team sizes**: Individual contributors to large enterprises
- **Development stages**: MVPs to production-ready releases
- **Workflows**: Feature development, bug fixes, documentation, releases

The key is proper configuration and understanding your project's context!

## Tips for Better Results

1. **Set up project context** - Always configure `.omni-dev/scopes.yaml`
2. **Use descriptive file patterns** - Help omni-dev understand your architecture
3. **Tune concurrency** - Use `--concurrency` for large commit ranges
4. **Review before applying** - Use `--save-only` for important changes
5. **Iterate and improve** - Update configuration based on results

For more detailed guidance, see:

- [User Guide](user-guide.md) - Complete usage instructions
- [Configuration Guide](configuration.md) - Setup details
- [Troubleshooting](troubleshooting.md) - Common issues and solutions
