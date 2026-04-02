package config

import (
	"fmt"
	"os"
	"sync"

	"go.uber.org/zap"
	"gopkg.in/yaml.v3"
)

// ── Schema ────────────────────────────────────────────────────────────────────
// Mirrors the Rust gateway-config schema exactly.
// When the Rust schema changes, update here too — the proto snapshot carries
// the YAML verbatim so the Rust side does the real deserialisation.

type GatewayConfig struct {
	Listener     ListenerConfig              `yaml:"listener"`
	Upstreams    map[string]UpstreamConfig   `yaml:"upstreams"`
	Routes       []RouteConfig               `yaml:"routes"`
	RateLimiting RateLimitConfig             `yaml:"rate_limiting"`
	Cache        CacheConfig                 `yaml:"cache"`
	Auth         AuthConfig                  `yaml:"auth"`
	Observability ObservabilityConfig        `yaml:"observability"`
	ControlPlane ControlPlaneServerConfig    `yaml:"control_plane"`
}

type ListenerConfig struct {
	Bind         string     `yaml:"bind"`
	MaxBodyBytes int        `yaml:"max_body_bytes"`
	Keepalive    int        `yaml:"keepalive"`
	TLS          *TLSConfig `yaml:"tls,omitempty"`
}

type TLSConfig struct {
	CertPath     string `yaml:"cert_path"`
	KeyPath      string `yaml:"key_path"`
	ClientCAPath string `yaml:"client_ca_path,omitempty"`
}

type UpstreamConfig struct {
	Endpoints      []EndpointConfig   `yaml:"endpoints"`
	LoadBalancing  LoadBalancingConfig `yaml:"load_balancing"`
	HealthCheck    *HealthCheckConfig  `yaml:"health_check,omitempty"`
	Timeouts       TimeoutConfig       `yaml:"timeouts"`
	Retry          *RetryConfig        `yaml:"retry,omitempty"`
}

type EndpointConfig struct {
	Address string `yaml:"address"`
	Weight  int    `yaml:"weight"`
	Active  bool   `yaml:"active"`
}

type LoadBalancingConfig struct {
	Algorithm    string  `yaml:"algorithm"`
	EwmaAlpha    float64 `yaml:"ewma_alpha,omitempty"`
	HashHeader   string  `yaml:"hash_header,omitempty"`
	VirtualNodes int     `yaml:"virtual_nodes,omitempty"`
}

type HealthCheckConfig struct {
	Path               string `yaml:"path"`
	Interval           int    `yaml:"interval"`
	Timeout            int    `yaml:"timeout"`
	HealthyThreshold   int    `yaml:"healthy_threshold"`
	UnhealthyThreshold int    `yaml:"unhealthy_threshold"`
}

type TimeoutConfig struct {
	Connect int `yaml:"connect"`
	Request int `yaml:"request"`
	Idle    int `yaml:"idle"`
}

type RetryConfig struct {
	MaxAttempts int   `yaml:"max_attempts"`
	BaseBackoff int   `yaml:"base_backoff"`
	OnStatuses  []int `yaml:"on_statuses"`
}

type RouteConfig struct {
	ID            string          `yaml:"id"`
	Match         RouteMatch      `yaml:"match_"`
	Upstream      string          `yaml:"upstream"`
	LoadBalancing *LoadBalancingConfig `yaml:"load_balancing,omitempty"`
	Middleware    MiddlewareConfig `yaml:"middleware"`
	Headers       HeaderMutations `yaml:"headers"`
}

type RouteMatch struct {
	Path    PathMatch         `yaml:"path"`
	Method  string            `yaml:"method,omitempty"`
	Headers map[string]string `yaml:"headers,omitempty"`
}

type PathMatch struct {
	Type    string `yaml:"type"`
	Value   string `yaml:"value,omitempty"`
	Pattern string `yaml:"pattern,omitempty"`
}

type MiddlewareConfig struct {
	Auth      *AuthMiddlewareConfig      `yaml:"auth,omitempty"`
	RateLimit *RateLimitMiddlewareRef    `yaml:"rate_limit,omitempty"`
	Cache     *CacheMiddlewareConfig     `yaml:"cache,omitempty"`
}

type AuthMiddlewareConfig struct {
	JWT    bool `yaml:"jwt"`
	APIKey bool `yaml:"api_key"`
	MTLS   bool `yaml:"mtls"`
}

type RateLimitMiddlewareRef struct {
	Policy string `yaml:"policy"`
}

type CacheMiddlewareConfig struct {
	TTL      int  `yaml:"ttl"`
	ReadOnly bool `yaml:"read_only"`
}

type HeaderMutations struct {
	RequestSet     map[string]string `yaml:"request_set,omitempty"`
	RequestRemove  []string          `yaml:"request_remove,omitempty"`
	ResponseSet    map[string]string `yaml:"response_set,omitempty"`
}

type RateLimitConfig struct {
	RedisURL string                    `yaml:"redis_url"`
	Policies map[string]RateLimitPolicy `yaml:"policies"`
}

type RateLimitPolicy struct {
	Local       TokenBucketConfig          `yaml:"local"`
	Distributed DistributedRateLimitConfig `yaml:"distributed"`
}

type TokenBucketConfig struct {
	Capacity   int `yaml:"capacity"`
	RefillRate int `yaml:"refill_rate"`
}

type DistributedRateLimitConfig struct {
	Algorithm   string `yaml:"algorithm"`
	Capacity    int    `yaml:"capacity,omitempty"`
	RefillRate  int    `yaml:"refill_rate,omitempty"`
	MaxRequests int    `yaml:"max_requests,omitempty"`
	Window      int    `yaml:"window,omitempty"`
}

type CacheConfig struct {
	RedisURL   string `yaml:"redis_url"`
	L1Capacity int    `yaml:"l1_capacity"`
}

type AuthConfig struct {
	JWT     JWTConfig     `yaml:"jwt"`
	APIKeys APIKeyConfig  `yaml:"api_keys"`
	MTLS    MTLSConfig    `yaml:"mtls"`
}

type JWTConfig struct {
	JWKSUrl       string   `yaml:"jwks_url"`
	Audience      []string `yaml:"audience"`
	Issuers       []string `yaml:"issuers"`
	JWKSCacheTTL  int      `yaml:"jwks_cache_ttl"`
}

type APIKeyConfig struct {
	RedisURL        string `yaml:"redis_url"`
	RedisKeyPrefix  string `yaml:"redis_key_prefix"`
}

type MTLSConfig struct {
	ClientCAPath string `yaml:"client_ca_path"`
	Required     bool   `yaml:"required"`
}

type ObservabilityConfig struct {
	MetricsBind string `yaml:"metrics_bind"`
}

type ControlPlaneServerConfig struct {
	GRPCAddress       string `yaml:"grpc_address"`
	ReconnectInterval int    `yaml:"reconnect_interval"`
}

// ── Loader ────────────────────────────────────────────────────────────────────

// Loader reads a YAML config file and exposes the current parsed config
// along with a version counter that increments on each successful reload.
type Loader struct {
	mu      sync.RWMutex
	path    string
	current *GatewayConfig
	rawYAML string
	version uint64
	log     *zap.Logger
}

func NewLoader(path string, log *zap.Logger) (*Loader, error) {
	l := &Loader{path: path, log: log}
	if err := l.reload(); err != nil {
		return nil, fmt.Errorf("initial config load: %w", err)
	}
	return l, nil
}

// Current returns the active config snapshot under a read lock.
func (l *Loader) Current() (*GatewayConfig, string, uint64) {
	l.mu.RLock()
	defer l.mu.RUnlock()
	return l.current, l.rawYAML, l.version
}

// Reload re-reads the config file. On error the old config stays active.
func (l *Loader) Reload() error {
	if err := l.reload(); err != nil {
		l.log.Warn("config reload failed — keeping current config", zap.Error(err))
		return err
	}
	l.log.Info("config reloaded",
		zap.String("path", l.path),
		zap.Uint64("version", l.version),
	)
	return nil
}

func (l *Loader) reload() error {
	raw, err := os.ReadFile(l.path)
	if err != nil {
		return fmt.Errorf("read file: %w", err)
	}

	var cfg GatewayConfig
	if err := yaml.Unmarshal(raw, &cfg); err != nil {
		return fmt.Errorf("yaml parse: %w", err)
	}

	if err := validate(&cfg); err != nil {
		return fmt.Errorf("validation: %w", err)
	}

	l.mu.Lock()
	defer l.mu.Unlock()
	l.current = &cfg
	l.rawYAML = string(raw)
	l.version++
	return nil
}

// ── Validation ────────────────────────────────────────────────────────────────

func validate(cfg *GatewayConfig) error {
	if cfg.Listener.Bind == "" {
		return fmt.Errorf("listener.bind is required")
	}
	for name, upstream := range cfg.Upstreams {
		if len(upstream.Endpoints) == 0 {
			return fmt.Errorf("upstream %q must have at least one endpoint", name)
		}
	}
	seen := make(map[string]bool)
	for _, route := range cfg.Routes {
		if route.ID == "" {
			return fmt.Errorf("every route must have a non-empty id")
		}
		if seen[route.ID] {
			return fmt.Errorf("duplicate route id %q", route.ID)
		}
		seen[route.ID] = true
		if _, ok := cfg.Upstreams[route.Upstream]; !ok {
			return fmt.Errorf("route %q references unknown upstream %q", route.ID, route.Upstream)
		}
	}
	return nil
}
