package k8s

import (
	"context"
	"fmt"
	"time"

	"go.uber.org/zap"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/client-go/informers"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/cache"
	"k8s.io/client-go/tools/clientcmd"
)

// ── ReloadFunc ────────────────────────────────────────────────────────────────

// ReloadFunc is called whenever the K8s watcher detects a change that
// might affect the gateway config (endpoint additions, removals, readiness
// state changes). The control plane re-reads the config and pushes a new
// snapshot to all connected Rust data plane instances.
type ReloadFunc func(reason string)

// ── Watcher ───────────────────────────────────────────────────────────────────

// Watcher runs K8s informers on Services and Endpoints in the configured
// namespaces and calls ReloadFunc on relevant changes.
//
// ## Why informers instead of polling?
//
// Informers are event-driven — they watch the K8s API server's watch stream
// and receive updates in real time without polling. They also maintain a
// local in-memory cache (the indexer) so we can list endpoints without
// hitting the API on every request.
//
// ## What triggers a reload?
//
// - Endpoint added or removed (pod scale-up/scale-down)
// - Endpoint readiness state change (pod becoming healthy/unhealthy)
// - Service created or deleted
//
// Port changes and label changes are treated as potential endpoint mutations
// and also trigger a reload.
type Watcher struct {
	client    kubernetes.Interface
	namespaces []string
	label     string          // optional label selector, e.g. "stateform.io/upstream=true"
	reload    ReloadFunc
	log       *zap.Logger
}

// NewWatcher builds a K8s client and watcher.
// Uses in-cluster config when running inside a pod; falls back to kubeconfig.
func NewWatcher(
	namespaces []string,
	label string,
	reload ReloadFunc,
	log *zap.Logger,
) (*Watcher, error) {
	cfg, err := rest.InClusterConfig()
	if err != nil {
		// Fall back to kubeconfig (local development)
		cfg, err = clientcmd.BuildConfigFromFlags("", clientcmd.RecommendedHomeFile)
		if err != nil {
			return nil, fmt.Errorf("k8s client config: %w", err)
		}
	}

	client, err := kubernetes.NewForConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("k8s client: %w", err)
	}

	return &Watcher{
		client:     client,
		namespaces: namespaces,
		label:      label,
		reload:     reload,
		log:        log,
	}, nil
}

// Run starts informers for all configured namespaces and blocks until ctx
// is cancelled. Each namespace gets its own SharedInformerFactory.
func (w *Watcher) Run(ctx context.Context) error {
	for _, ns := range w.namespaces {
		if err := w.startNamespace(ctx, ns); err != nil {
			return fmt.Errorf("start watcher for namespace %q: %w", ns, err)
		}
	}

	<-ctx.Done()
	w.log.Info("k8s watcher stopped")
	return nil
}

func (w *Watcher) startNamespace(ctx context.Context, namespace string) error {
	factory := informers.NewSharedInformerFactoryWithOptions(
		w.client,
		// Resync every 5 minutes — catches any events missed during transient
		// disconnects from the API server
		5*time.Minute,
		informers.WithNamespace(namespace),
	)

	// ── Endpoints informer ────────────────────────────────────────────────────
	endpointsInformer := factory.Core().V1().Endpoints().Informer()
	endpointsInformer.AddEventHandler(cache.ResourceEventHandlerFuncs{
		AddFunc: func(obj interface{}) {
			ep := obj.(*corev1.Endpoints)
			if !w.matchesLabel(ep.Labels) { return }
			w.log.Info("endpoints added",
				zap.String("namespace", ep.Namespace),
				zap.String("name", ep.Name),
			)
			w.reload("k8s endpoints added: " + ep.Namespace + "/" + ep.Name)
		},
		UpdateFunc: func(old, new interface{}) {
			ep := new.(*corev1.Endpoints)
			if !w.matchesLabel(ep.Labels) { return }
			if !endpointsChanged(old.(*corev1.Endpoints), ep) { return }
			w.log.Info("endpoints updated",
				zap.String("namespace", ep.Namespace),
				zap.String("name", ep.Name),
			)
			w.reload("k8s endpoints updated: " + ep.Namespace + "/" + ep.Name)
		},
		DeleteFunc: func(obj interface{}) {
			ep, ok := obj.(*corev1.Endpoints)
			if !ok { return }
			if !w.matchesLabel(ep.Labels) { return }
			w.log.Info("endpoints deleted",
				zap.String("namespace", ep.Namespace),
				zap.String("name", ep.Name),
			)
			w.reload("k8s endpoints deleted: " + ep.Namespace + "/" + ep.Name)
		},
	})

	// ── Services informer ─────────────────────────────────────────────────────
	servicesInformer := factory.Core().V1().Services().Informer()
	servicesInformer.AddEventHandler(cache.ResourceEventHandlerFuncs{
		AddFunc: func(obj interface{}) {
			svc := obj.(*corev1.Service)
			if !w.matchesLabel(svc.Labels) { return }
			w.log.Info("service added",
				zap.String("namespace", svc.Namespace),
				zap.String("name", svc.Name),
			)
			w.reload("k8s service added: " + svc.Namespace + "/" + svc.Name)
		},
		DeleteFunc: func(obj interface{}) {
			svc, ok := obj.(*corev1.Service)
			if !ok { return }
			if !w.matchesLabel(svc.Labels) { return }
			w.log.Info("service deleted",
				zap.String("namespace", svc.Namespace),
				zap.String("name", svc.Name),
			)
			w.reload("k8s service deleted: " + svc.Namespace + "/" + svc.Name)
		},
	})

	factory.Start(ctx.Done())

	// Wait for initial cache sync before calling reload for the first time.
	// This prevents the gateway from starting with an empty endpoint list.
	w.log.Info("waiting for k8s cache sync", zap.String("namespace", namespace))
	if !cache.WaitForCacheSync(ctx.Done(),
		endpointsInformer.HasSynced,
		servicesInformer.HasSynced,
	) {
		return fmt.Errorf("cache sync timed out for namespace %q", namespace)
	}
	w.log.Info("k8s cache synced", zap.String("namespace", namespace))

	return nil
}

// matchesLabel returns true when the label filter is empty (match all) or
// the object's labels match the configured selector.
func (w *Watcher) matchesLabel(objLabels map[string]string) bool {
	if w.label == "" {
		return true
	}
	sel, err := labels.Parse(w.label)
	if err != nil {
		return true // misconfigured selector — don't silently drop events
	}
	return sel.Matches(labels.Set(objLabels))
}

// endpointsChanged returns true when the ready addresses or ports changed —
// the subset of endpoint data that affects gateway routing.
func endpointsChanged(old, new *corev1.Endpoints) bool {
	if len(old.Subsets) != len(new.Subsets) {
		return true
	}
	for i := range old.Subsets {
		if i >= len(new.Subsets) { return true }
		if len(old.Subsets[i].Addresses) != len(new.Subsets[i].Addresses) { return true }
		if len(old.Subsets[i].Ports) != len(new.Subsets[i].Ports) { return true }
	}
	return false
}
