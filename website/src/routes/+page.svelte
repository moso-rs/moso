<script lang="ts">
	import { SITE } from '$lib/nav';
	import { href } from '$lib/links';
	import InstallCommand from '$lib/components/InstallCommand.svelte';
	import Prose from '$lib/components/Prose.svelte';
	import { html as endpointSample } from '$content/landing/endpoint.md';

	const features = [
		{
			title: 'Schemas that do four jobs',
			body: 'One `#[derive(Schema)]` gives you deserialisation, validation, a JSON Schema and the OpenAPI component. Constrained types like `Email` and `Slug` parse rather than check.',
			href: '/docs/guides/schemas'
		},
		{
			title: 'OpenAPI with no annotations',
			body: 'The document is assembled at boot from handler signatures, so it cannot drift. Export it, diff it in CI, and fail the build on a breaking change.',
			href: '/docs/guides/openapi'
		},
		{
			title: 'An ORM built for review',
			body: 'A shape-stable query builder over a sealed SQL facade, typed columns, relations that will not N plus one, and migrations you generate then read before running.',
			href: '/docs/guides/relations'
		},
		{
			title: 'Authentication, whole',
			body: 'Sessions, passwords, JWT, API keys, OAuth, passkeys and multi-factor, with the account lifecycle around them rather than a login endpoint and a shrug.',
			href: '/docs/guides/authentication'
		},
		{
			title: 'Authorisation you can explain',
			body: 'A typed permission registry, roles, resource policies and query level scoping, with an explain trace that says exactly why a request was refused.',
			href: '/docs/guides/permissions'
		},
		{
			title: 'Background work',
			body: 'Jobs on Postgres or Redis, transactional enqueue, retries with backoff, a dead letter queue and cron with leader election.',
			href: '/docs/guides/jobs'
		},
		{
			title: 'Cache, mail and storage',
			body: 'Typed key value namespaces, rate limits and locks, a mailer with templates and previews, and object storage with presigned uploads.',
			href: '/docs/guides/cache'
		},
		{
			title: 'Tests that boot the real app',
			body: 'A test harness that starts your application, hands you a typed client, templates a fresh database per test and diffs snapshots.',
			href: '/docs/guides/testing'
		}
	];

	const pillars = [
		{
			title: 'One definition, not four',
			body: 'The thing FastAPI got right is that a type is enough. Moso derives request parsing, validation, error shape and documentation from the same declaration, so there is no second description of your API to keep in sync.'
		},
		{
			title: 'The batteries are the framework',
			body: 'A web framework that stops at routing leaves you to assemble the other ninety percent. Moso ships the data layer, auth, jobs, cache, mail and storage as one versioned whole, tested together.'
		},
		{
			title: 'Errors a human wrote',
			body: 'Trait bound vomit is the main reason people bounce off Rust frameworks. Diagnostics here are a product surface with an owner and a regression suite, and every public trait carries a written failure message.'
		}
	];
</script>

<svelte:head>
	<title>{SITE.name}: {SITE.tagline}</title>
	<meta name="description" content={SITE.description} />
	<meta property="og:title" content="{SITE.name}: {SITE.tagline}" />
	<meta property="og:description" content={SITE.description} />
	<link rel="canonical" href="{SITE.url}/" />
</svelte:head>

<div class="landing">
	<section class="hero">
		<div class="hero-copy">
			<p class="hero-eyebrow">Batteries included, model driven, Rust</p>
			<h1 class="hero-title">
				Describe the type.<br />
				Ship the API.
			</h1>
			<p class="hero-lede">
				Moso reads one Rust type and gives you parsing, validation, error responses and OpenAPI
				from it. The ORM, migrations, authentication, authorisation, jobs, cache, mail and
				storage are already in the box.
			</p>
			<div class="hero-actions">
				<a class="button button-primary" href={href('/docs/start/quick-start')}>Get started</a>
				<a class="button button-secondary" href={href('/docs/guides')}>
					How it works
				</a>
			</div>
			<InstallCommand />
		</div>

		<div class="hero-code">
			<Prose html={endpointSample} />
			<p class="hero-code-note">
				The body is parsed, validated and rejected with a problem document before
				<code>create</code> runs. The OpenAPI operation, its request schema, its
				<code>201</code> response and its tag all come from this signature.
			</p>
		</div>
	</section>

	<section class="strip" aria-label="At a glance">
		<div class="strip-inner">
			<span>Axum and Tower underneath</span>
			<span aria-hidden="true">·</span>
			<span>OpenAPI 3.1</span>
			<span aria-hidden="true">·</span>
			<span>RFC 9457 errors</span>
			<span aria-hidden="true">·</span>
			<span>Postgres and SQLite</span>
			<span aria-hidden="true">·</span>
			<span>No unsafe code</span>
		</div>
	</section>

	<section class="pillars">
		{#each pillars as pillar}
			<article class="pillar">
				<h2>{pillar.title}</h2>
				<p>{pillar.body}</p>
			</article>
		{/each}
	</section>

	<section class="features">
		<header class="features-header">
			<h2>Everything a real application needs</h2>
			<p>
				Each of these is a crate in the workspace, tested against the others, released as one
				version. Follow any card for the guide that shows the code.
			</p>
		</header>
		<div class="feature-grid">
			{#each features as feature}
				<a class="feature-card" href={href(feature.href)}>
					<h3>{feature.title}</h3>
					<p>{feature.body}</p>
				</a>
			{/each}
		</div>
	</section>

	<section class="honesty">
		<h2>What is actually built</h2>
		<p>
			Moso is unreleased and pre 1.0. The HTTP layer, the ORM, migrations, the cache, auth,
			authorisation, jobs, mail and storage all exist as workspace crates with passing tests.
		</p>
		<p>
			Pages that describe something unfinished say so at the top.
		</p>
	</section>
</div>
